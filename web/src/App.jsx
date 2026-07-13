import React, { useEffect, useRef, useState } from 'react'
import { api, eventStream, getToken, setToken } from './api.js'

// Progressive disclosure: novices see a clean, small surface; power users
// flip "Advanced" to reveal the deep tabs. The split is by who needs it, not
// by how hard it is — everyday use (talk, approve, add skills, pick a voice)
// stays visible; the machinery (tools, subagents, loops, spend, memory) hides
// until asked for.
const SIMPLE_TABS = ['chat', 'approvals', 'skills', 'personalities', 'settings']
const ADVANCED_TABS = ['tools', 'subagents', 'loops', 'spend', 'memory']

// Minimal, safe markdown -> HTML for assistant messages. Escapes first (no
// injection), protects code so its contents aren't re-interpreted, then applies
// a small, well-formed subset: code blocks, inline code, bold, links, headers,
// and bullet lists. Only http(s) links are emitted.
function mdToHtml(src) {
  const esc = (s) =>
    s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
  const code = []
  // Park code as a sentinel token that survives esc() (no & < >) and will not
  // collide with prose. Restored verbatim at the end.
  const park = (html) => `@@RVC${code.push(html) - 1}@@`
  let t = String(src)
    .replace(/```[\w+.-]*\n?([\s\S]*?)```/g, (_, c) => park(`<pre><code>${esc(c)}</code></pre>`))
    .replace(/`([^`\n]+)`/g, (_, c) => park(`<code>${esc(c)}</code>`))
  t = esc(t)
    .replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>')
    .replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>')
    .replace(/^\s*#{1,6}\s+(.*)$/gm, '<h4>$1</h4>')
  // Bullet lists: wrap consecutive "- "/"* " lines in <ul>.
  t = t.replace(/(?:^\s*[-*]\s+.*(?:\n|$))+/gm, (block) => {
    const items = block
      .trim()
      .split('\n')
      .map((l) => `<li>${l.replace(/^\s*[-*]\s+/, '')}</li>`)
      .join('')
    return `<ul>${items}</ul>`
  })
  t = t.replace(/\n/g, '<br>')
  code.forEach((frag, i) => {
    t = t.split(`@@RVC${i}@@`).join(frag)
  })
  return t
}

export default function App() {
  const [authed, setAuthed] = useState(false)
  const [tab, setTab] = useState('chat')
  const [pendingCount, setPendingCount] = useState(0)
  const [banner, setBanner] = useState(null)
  const [advanced, setAdvanced] = useState(() => localStorage.getItem('rev_advanced') === '1')

  useEffect(() => {
    if (!getToken()) return
    api
      .health()
      .then(() => setAuthed(true))
      .catch(() => setAuthed(false))
  }, [])

  // If the user hasn't chosen a surface yet, adopt the wizard's default
  // (config.power_user). Once they flip the toggle, localStorage wins forever.
  useEffect(() => {
    if (!authed || localStorage.getItem('rev_advanced') !== null) return
    api.config().then((c) => { if (c.power_user) setAdvanced(true) }).catch(() => {})
  }, [authed])

  const toggleAdvanced = () => {
    const next = !advanced
    setAdvanced(next)
    localStorage.setItem('rev_advanced', next ? '1' : '0')
    // If we're hiding the tab we're on, fall back to chat.
    if (!next && ADVANCED_TABS.includes(tab)) setTab('chat')
  }

  if (!authed) return <Login onAuthed={() => setAuthed(true)} />

  const tabs = advanced ? [...SIMPLE_TABS, ...ADVANCED_TABS] : SIMPLE_TABS

  return (
    <div className="shell">
      <header>
        <img className="brand-logo" src="/logo.png" alt="" />
        <span className="brand">revenant</span>
        <nav>
          {tabs.map((name) => (
            <button
              key={name}
              className={tab === name ? 'tab active' : 'tab'}
              onClick={() => setTab(name)}
            >
              {name}
              {name === 'approvals' && pendingCount > 0 && (
                <span className="badge">{pendingCount}</span>
              )}
            </button>
          ))}
          <button
            className={advanced ? 'tab adv-toggle on' : 'tab adv-toggle'}
            onClick={toggleAdvanced}
            title={advanced ? 'Hide advanced tabs' : 'Show advanced tabs (tools, subagents, loops, spend, memory)'}
          >
            {advanced ? '⚙ advanced ✓' : '⚙ advanced'}
          </button>
        </nav>
      </header>
      {banner && (
        <div className="banner" onClick={() => setBanner(null)}>
          {banner}
        </div>
      )}
      <main>
        {tab === 'chat' && <Chat onApprovalCount={setPendingCount} setBanner={setBanner} />}
        {tab === 'approvals' && <Approvals onCount={setPendingCount} />}
        {tab === 'skills' && <Skills />}
        {tab === 'tools' && <Tools />}
        {tab === 'subagents' && <Subagents />}
        {tab === 'personalities' && <Personalities />}
        {tab === 'loops' && <Loops />}
        {tab === 'spend' && <Spend />}
        {tab === 'memory' && <Memory />}
        {tab === 'settings' && <Settings />}
      </main>
    </div>
  )
}

function Login({ onAuthed }) {
  const [value, setValue] = useState('')
  const [error, setError] = useState(null)
  const submit = async () => {
    setToken(value)
    try {
      await api.health()
      onAuthed()
    } catch {
      setError('token rejected — find it in ~/.revenant/token')
    }
  }
  return (
    <div className="login">
      <img className="login-logo" src="/logo.png" alt="revenant" />
      <h1>revenant</h1>
      <p>paste the control token from ~/.revenant/token</p>
      <input
        type="password"
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => e.key === 'Enter' && submit()}
        placeholder="token"
        autoFocus
      />
      <button onClick={submit}>connect</button>
      {error && <p className="error">{error}</p>}
    </div>
  )
}

// ---- chat ----

function Chat({ onApprovalCount, setBanner }) {
  const [sessions, setSessions] = useState([])
  const [sessionId, setSessionId] = useState(null)
  const [messages, setMessages] = useState([])
  const [input, setInput] = useState('')
  const [streaming, setStreaming] = useState(false)
  const [approval, setApproval] = useState(null)
  const [personas, setPersonas] = useState([])
  const [persona, setPersona] = useState('')
  const bottom = useRef(null)

  const refreshSessions = () => api.sessions().then((r) => setSessions(r.sessions))

  useEffect(() => {
    refreshSessions()
    api.createSession('web').then((r) => setSessionId(r.id))
    api.personalities().then((r) => setPersonas(r.personalities))
  }, [])

  useEffect(() => {
    if (!sessionId) return
    api.messages(sessionId).then((r) =>
      setMessages(
        r.messages.flatMap((m) =>
          m.content
            .filter((b) => b.type === 'text' && b.text)
            .map((b) => ({ role: m.role, text: b.text }))
        )
      )
    )
  }, [sessionId])

  useEffect(() => {
    if (!sessionId) return
    const source = eventStream((type, event) => {
      const mine = event.session_id === sessionId
      switch (type) {
        case 'turn_delta':
          if (!mine) break
          setStreaming(true)
          setMessages((prev) => {
            const next = [...prev]
            const last = next[next.length - 1]
            if (last && last.role === 'assistant' && last.live) {
              next[next.length - 1] = { ...last, text: last.text + event.text }
            } else {
              next.push({ role: 'assistant', text: event.text, live: true })
            }
            return next
          })
          break
        case 'tool_started':
          if (mine) {
            setMessages((prev) => [...prev, { role: 'tool', text: event.summary }])
          }
          break
        case 'turn_completed':
          if (mine) {
            setStreaming(false)
            setMessages((prev) =>
              prev.map((m) => (m.live ? { ...m, live: false } : m))
            )
          }
          break
        case 'turn_failed':
          if (mine) {
            setStreaming(false)
            setMessages((prev) => [...prev, { role: 'error', text: event.error }])
          }
          break
        case 'turn_cancelled':
          if (mine) {
            setStreaming(false)
            setMessages((prev) => [
              ...prev.map((m) => (m.live ? { ...m, live: false } : m)),
              { role: 'tool', text: '🛑 stopped' },
            ])
          }
          break
        case 'approval_created':
          setApproval(event)
          onApprovalCount((c) => c + 1)
          break
        case 'approval_resolved':
          setApproval((current) => (current && current.id === event.id ? null : current))
          onApprovalCount((c) => Math.max(0, c - 1))
          break
        case 'gateway_status':
          if (!event.healthy) setBanner('gateway unhealthy')
          break
        case 'budget_alert':
          // Global (no session_id) — show regardless of the open session.
          setBanner(`💸 ${event.spent} spent today · ${event.pct}% of ${event.budget} daily budget`)
          break
        default:
      }
    })
    return () => source.close()
  }, [sessionId])

  useEffect(() => bottom.current?.scrollIntoView({ behavior: 'smooth' }), [messages])

  const send = async () => {
    const text = input.trim()
    if (!text || !sessionId) return
    setInput('')
    setMessages((prev) => [...prev, { role: 'user', text }])
    try {
      await api.send(sessionId, text)
    } catch (err) {
      setMessages((prev) => [...prev, { role: 'error', text: String(err) }])
    }
  }

  const stop = async () => {
    if (!sessionId) return
    try {
      await api.cancel(sessionId)
    } catch {
      /* best-effort; the SSE turn_cancelled event flips streaming off */
    }
  }

  return (
    <div className="chat">
      <aside>
        <button className="newchat" onClick={() => api.createSession(`web-${Date.now()}`).then((r) => { setSessionId(r.id); refreshSessions() })}>
          + new
        </button>
        {sessions.map((s) => (
          <div
            key={s.id}
            className={s.id === sessionId ? 'session active' : 'session'}
            onClick={() => setSessionId(s.id)}
          >
            <span>#{s.id}</span> {s.channel}/{s.peer}
            <small>{s.message_count} msgs</small>
          </div>
        ))}
      </aside>
      <section>
        <div className="chat-bar">
          <select
            value={persona}
            onChange={(e) => {
              const val = e.target.value
              setPersona(val)
              if (sessionId) api.setPersona(sessionId, val || null)
            }}
            title="personality (voice)"
          >
            <option value="">default voice</option>
            {personas.map((p) => (
              <option key={p.name} value={p.name}>
                {p.emoji} {p.name}
              </option>
            ))}
          </select>
        </div>
        <div className="log">
          {messages.map((m, i) => (
            <div key={i} className={`msg ${m.role}`}>
              {m.role === 'tool' ? (
                `⚙ ${m.text}`
              ) : m.role === 'assistant' ? (
                <span dangerouslySetInnerHTML={{ __html: mdToHtml(m.text) }} />
              ) : (
                m.text
              )}
              {m.live && <span className="cursor">▌</span>}
            </div>
          ))}
          <div ref={bottom} />
        </div>
        {approval && (
          <div className="approval">
            <span>⚠ {approval.summary}</span>
            <button className="ok" onClick={() => api.decide(approval.id, true)}>
              approve
            </button>
            <button className="no" onClick={() => api.decide(approval.id, false)}>
              deny
            </button>
          </div>
        )}
        <div className="composer">
          <input
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && send()}
            placeholder={streaming ? 'streaming…' : 'message revenant'}
            autoFocus
          />
          {streaming ? (
            <button className="stop" onClick={stop} title="stop the running turn">
              stop
            </button>
          ) : (
            <button onClick={send}>send</button>
          )}
        </div>
      </section>
    </div>
  )
}

// ---- approvals ----

function Approvals({ onCount }) {
  const [items, setItems] = useState([])
  const refresh = () =>
    api.approvals().then((r) => {
      setItems(r.approvals)
      onCount(r.approvals.length)
    })
  useEffect(() => {
    refresh()
    const timer = setInterval(refresh, 4000)
    return () => clearInterval(timer)
  }, [])

  const decide = async (id, approve) => {
    await api.decide(id, approve)
    refresh()
  }

  if (items.length === 0) return <div className="empty">no pending approvals</div>
  return (
    <div className="list">
      {items.map((a) => {
        let summary = a.kind
        try {
          summary = JSON.parse(a.payload).summary || a.kind
        } catch {}
        return (
          <div key={a.id} className="card">
            <div className="card-title">⚠ {summary}</div>
            <div className="card-meta">
              requested {new Date(a.requested_at * 1000).toLocaleTimeString()}
            </div>
            <div className="card-actions">
              <button className="ok" onClick={() => decide(a.id, true)}>
                approve
              </button>
              <button className="no" onClick={() => decide(a.id, false)}>
                deny
              </button>
            </div>
          </div>
        )
      })}
    </div>
  )
}

// ---- spend ----

function Spend() {
  const [window, setWindow] = useState('today')
  const [rows, setRows] = useState([])
  const [gw, setGw] = useState(null) // gateway analytics (authoritative)
  const [budget, setBudget] = useState(null) // daily budget gauge, or null
  useEffect(() => {
    api.spend(window).then((r) => setRows(r.by_model))
  }, [window])
  useEffect(() => {
    api.analytics().then(setGw).catch(() => setGw({ available: false, error: 'unreachable' }))
  }, [])
  useEffect(() => {
    // Daily budget gauge — computed server-side (same math as the alert).
    api.budget()
      .then((b) => setBudget(b && b.configured ? b : null))
      .catch(() => setBudget(null))
  }, [])

  const storeMax = Math.max(1, ...rows.map((r) => r.tokens_in + r.tokens_out))
  const gwMax = Math.max(1, ...((gw?.by_provider) || []).map((g) => g.total_tokens))
  return (
    <div className="spend">
      {budget && (
        <div className={`budget-gauge${budget.frac >= 1 ? ' over' : budget.frac >= 0.8 ? ' warn' : ''}`}>
          <div className="budget-head">
            <span>daily budget</span>
            <span className="budget-nums">{budget.spent} / {budget.budget} · {budget.pct}%</span>
          </div>
          <div className="bar-track">
            <div className="bar in" style={{ width: `${Math.min(100, budget.frac * 100)}%` }} />
          </div>
        </div>
      )}
      {/* Gateway-authoritative view: what agentgateway actually metered. */}
      <h3 className="spend-head">Gateway · authoritative <span className="muted">· last 24h</span></h3>
      {gw && gw.available && gw.by_provider.length > 0 && (
        <>
          {gw.by_provider.map((g) => (
            <div key={g.label} className="bar-row">
              <span className="bar-label">{g.label}</span>
              <div className="bar-track">
                <div className="bar in" style={{ width: `${(g.total_tokens / gwMax) * 100}%` }} />
              </div>
              <span className="bar-nums">
                {g.total_tokens.toLocaleString()} tok · {g.requests} req · ${g.cost.toFixed(4)}
              </span>
            </div>
          ))}
          <div className="bar-row total">
            <span className="bar-label">total</span>
            <div className="bar-track" />
            <span className="bar-nums">
              {gw.totals.total_tokens.toLocaleString()} tok · {gw.totals.requests} req · $
              {gw.totals.cost.toFixed(4)}
            </span>
          </div>
        </>
      )}
      {gw && gw.available && gw.by_provider.length === 0 && (
        <div className="empty">no gateway traffic in the last 24h</div>
      )}
      {gw && !gw.available && (
        <div className="empty">gateway analytics unavailable ({gw.error || 'gateway down'})</div>
      )}

      {/* Harness bookkeeping (per-response usage the agent recorded). */}
      <h3 className="spend-head">Harness <span className="muted">· by model</span></h3>
      <div className="controls">
        {['today', '24h', '7d'].map((w) => (
          <button key={w} className={w === window ? 'tab active' : 'tab'} onClick={() => setWindow(w)}>
            {w}
          </button>
        ))}
      </div>
      {rows.length === 0 && <div className="empty">no spend in this window</div>}
      {rows.map((r) => (
        <div key={r.model} className="bar-row">
          <span className="bar-label">{r.model}</span>
          <div className="bar-track">
            <div
              className="bar in"
              style={{ width: `${(r.tokens_in / storeMax) * 100}%` }}
              title={`${r.tokens_in.toLocaleString()} in`}
            />
            <div
              className="bar out"
              style={{ width: `${(r.tokens_out / storeMax) * 100}%` }}
              title={`${r.tokens_out.toLocaleString()} out`}
            />
          </div>
          <span className="bar-nums">
            {r.tokens_in.toLocaleString()} in · {r.tokens_out.toLocaleString()} out ·{' '}
            {r.requests} calls
          </span>
        </div>
      ))}
      <p className="hint">
        Gateway numbers are metered below the harness (authoritative). Harness numbers are the
        agent's own per-response tallies — they can differ (e.g. pre-gateway history).
      </p>
    </div>
  )
}

// ---- skills ----

function Skills() {
  const [skills, setSkills] = useState([])
  useEffect(() => {
    api.skills().then((r) => setSkills(r.skills))
  }, [])
  return (
    <div className="list">
      <div className="card-meta">
        agentskills.io SKILL.md folders under ~/.revenant/skills — the agent
        loads a skill's full instructions on demand, and can author its own.
      </div>
      {skills.length === 0 && <div className="empty">no skills installed</div>}
      {skills.map((s) => (
        <div key={s.name} className="card">
          <div className="card-title">{s.name}</div>
          <div>{s.description}</div>
        </div>
      ))}
    </div>
  )
}

// ---- tools ----

const TIER_COLOR = {
  ReadOnly: '#34d399',
  WriteWorkspace: '#a78bfa',
  Network: '#60a5fa',
  Dangerous: '#f87171',
}

function Tools() {
  const [tools, setTools] = useState([])
  useEffect(() => {
    api.tools().then((r) => setTools(r.tools))
  }, [])
  // Group by permission tier, ordered least→most privileged.
  const order = ['ReadOnly', 'WriteWorkspace', 'Network', 'Dangerous']
  const groups = order
    .map((tier) => ({ tier, items: tools.filter((t) => t.permission === tier) }))
    .filter((g) => g.items.length > 0)
  return (
    <div className="list">
      <div className="card-meta">
        built-in tools by permission tier. Dangerous tools require owner
        approval on every call; MCP-server tools join this list in a later
        milestone.
      </div>
      {groups.map((g) => (
        <div key={g.tier} className="card">
          <div className="card-title">
            <span className="pill" style={{ background: TIER_COLOR[g.tier] }}>
              {g.tier}
            </span>
          </div>
          {g.items.map((t) => (
            <div key={t.name} className="tool-row">
              <b>{t.name}</b>
              <span>{t.description}</span>
            </div>
          ))}
        </div>
      ))}
    </div>
  )
}

// ---- subagents: editable roster + run history ----

const BLANK_AGENT = {
  name: '',
  description: '',
  tier: 'fast',
  tools: [],
  skills: [],
  directive: '',
}

function Subagents() {
  const [defs, setDefs] = useState([])
  const [subs, setSubs] = useState([])
  const [live, setLive] = useState([])
  const [editing, setEditing] = useState(null) // AgentDef being edited, or null
  const [allTools, setAllTools] = useState([])

  const refreshDefs = () => api.agents().then((r) => setDefs(r.agents))
  const refreshRuns = () => api.subagents().then((r) => setSubs(r.subagents))

  useEffect(() => {
    refreshDefs()
    refreshRuns()
    api.tools().then((r) => setAllTools(r.tools.map((t) => t.name)))
    const source = eventStream((type, event) => {
      if (type === 'subagent_spawned') {
        setLive((prev) => [
          { ...event, at: Date.now() },
          ...prev.filter((s) => s.child_session !== event.child_session),
        ])
      }
      if (type === 'subagent_finished') {
        setLive((prev) => prev.filter((s) => s.child_session !== event.child_session))
        setTimeout(refreshRuns, 500)
      }
    })
    return () => source.close()
  }, [])

  const openEditor = async (name) => {
    if (name) {
      const def = await api.agent(name)
      setEditing({ ...BLANK_AGENT, ...def, tools: def.tools || [], skills: def.skills || [] })
    } else {
      setEditing({ ...BLANK_AGENT })
    }
  }

  const save = async () => {
    if (!editing.name.trim() || !editing.directive.trim()) return
    await api.saveAgent(editing.name, {
      description: editing.description,
      tier: editing.tier,
      tools: editing.tools,
      skills: editing.skills,
      directive: editing.directive,
    })
    setEditing(null)
    refreshDefs()
  }

  if (editing) {
    return (
      <AgentEditor
        agent={editing}
        setAgent={setEditing}
        allTools={allTools}
        onSave={save}
        onCancel={() => setEditing(null)}
      />
    )
  }

  return (
    <div className="list">
      <div className="row-between">
        <div className="card-meta">
          named subagents the main agent can delegate to. Revenant can draft
          these; you own the directive, tools, tier, and skills. Files live in
          ~/.revenant/agents/*.md.
        </div>
        <button className="newchat" style={{ width: 'auto' }} onClick={() => openEditor(null)}>
          + define agent
        </button>
      </div>

      <div className="section-label">defined agents</div>
      {defs.length === 0 && <div className="empty">none defined yet</div>}
      {defs.map((a) => (
        <div key={a.name} className="card clickable" onClick={() => openEditor(a.name)}>
          <div className="card-title">
            {a.name}
            <span className="pill sm" style={{ background: TIER_COLOR.ReadOnly, marginLeft: 8 }}>
              {a.tier || 'fast'}
            </span>
          </div>
          <div>{a.description}</div>
          <div className="card-meta">
            tools: {a.tools?.length ? a.tools.join(', ') : 'all (inherited)'}
            {a.skills?.length ? ` · skills: ${a.skills.join(', ')}` : ''}
          </div>
        </div>
      ))}

      <div className="section-label">recent runs</div>
      {live.map((s) => (
        <div key={s.child_session} className="card running">
          <div className="card-title">
            <span className="spinner">◍</span> #{s.child_session} · {s.tier}
            <small> from #{s.parent_session}</small>
          </div>
          <div>{s.task}</div>
        </div>
      ))}
      {subs.length === 0 && live.length === 0 && (
        <div className="empty">no runs yet — ask the agent to delegate something</div>
      )}
      {subs.map((s) => (
        <div key={s.id} className="card">
          <div className="card-title">
            #{s.id}
            <small> from #{s.parent_session} · {s.messages} msgs</small>
          </div>
          <div>{s.task}</div>
          <div className="card-meta">{new Date(s.created_at * 1000).toLocaleString()}</div>
        </div>
      ))}
    </div>
  )
}

function AgentEditor({ agent, setAgent, allTools, onSave, onCancel }) {
  const toggleTool = (name) => {
    setAgent((a) => ({
      ...a,
      tools: a.tools.includes(name)
        ? a.tools.filter((t) => t !== name)
        : [...a.tools, name],
    }))
  }
  return (
    <div className="list">
      <div className="section-label">
        {agent.name ? `edit ${agent.name}` : 'new subagent'}
      </div>
      <label className="field">
        <span>name</span>
        <input
          value={agent.name}
          onChange={(e) => setAgent({ ...agent, name: e.target.value })}
          placeholder="researcher"
        />
      </label>
      <label className="field">
        <span>description</span>
        <input
          value={agent.description}
          onChange={(e) => setAgent({ ...agent, description: e.target.value })}
          placeholder="what this agent is for (shown to the main agent)"
        />
      </label>
      <label className="field">
        <span>tier</span>
        <select value={agent.tier || 'fast'} onChange={(e) => setAgent({ ...agent, tier: e.target.value })}>
          {['fast', 'balanced', 'deep', 'local'].map((t) => (
            <option key={t} value={t}>{t}</option>
          ))}
        </select>
      </label>
      <div className="field">
        <span>tools (none = inherit all)</span>
        <div className="tool-picker">
          {allTools.map((t) => (
            <button
              key={t}
              className={agent.tools.includes(t) ? 'chip on' : 'chip'}
              onClick={() => toggleTool(t)}
            >
              {t}
            </button>
          ))}
        </div>
      </div>
      <label className="field">
        <span>directive (the agent's instructions)</span>
        <textarea
          rows={10}
          value={agent.directive}
          onChange={(e) => setAgent({ ...agent, directive: e.target.value })}
          placeholder="You are a focused research subagent. Given a topic, search memory and files…"
        />
      </label>
      <div className="card-actions">
        <button className="ok" onClick={onSave}>save</button>
        <button className="no" onClick={onCancel}>cancel</button>
      </div>
    </div>
  )
}

// ---- personalities ----

function Personalities() {
  const [items, setItems] = useState([])
  useEffect(() => {
    api.personalities().then((r) => setItems(r.personalities))
  }, [])
  return (
    <div className="list">
      <div className="card-meta">
        selectable voices. Pick one per chat with the dropdown above the
        composer (or /persona in chat/Telegram). Voice only — a personality
        flavors tone but never changes what the agent can do or its safety
        rules. Files live in ~/.revenant/personalities/*.md; the agent can
        draft new ones with persona_create.
      </div>
      {items.length === 0 && <div className="empty">no personalities</div>}
      {items.map((p) => (
        <div key={p.name} className="card">
          <div className="card-title">
            <span style={{ marginRight: 6 }}>{p.emoji}</span>
            {p.name}
          </div>
          <div>{p.description}</div>
          <div className="loop-prompt">{p.voice}</div>
        </div>
      ))}
    </div>
  )
}

// ---- loops ----

function Loops() {
  const [loops, setLoops] = useState([])
  const [runs, setRuns] = useState({})
  const refresh = () => api.loops().then((r) => setLoops(r.loops))
  useEffect(() => {
    refresh()
    const source = eventStream((type) => {
      if (type === 'loop_completed') setTimeout(refresh, 400)
    })
    const timer = setInterval(refresh, 8000)
    return () => {
      source.close()
      clearInterval(timer)
    }
  }, [])

  const showRuns = async (id) => {
    const r = await api.loopRuns(id)
    setRuns((prev) => ({ ...prev, [id]: r.runs }))
  }

  return (
    <div className="list">
      <div className="card-meta">
        recurring jobs the agent runs on a schedule (heartbeats, watches,
        digests). It creates and tunes these itself via loop_create; you can
        pause or delete any. Results can push to Telegram.
      </div>
      {loops.length === 0 && (
        <div className="empty">
          no loops yet — ask the agent to "check X every 10 minutes"
        </div>
      )}
      {loops.map((l) => (
        <div key={l.id} className="card">
          <div className="card-title">
            {l.name}
            <span
              className="pill sm"
              style={{ background: l.enabled ? '#34d399' : '#7a828d', marginLeft: 8 }}
            >
              {l.enabled ? 'active' : 'paused'}
            </span>
            <small>{l.schedule} · {l.tier}{l.channel_out ? ` → ${l.channel_out}` : ''}</small>
          </div>
          <div className="loop-prompt">{l.prompt}</div>
          <div className="card-meta">
            {l.last_run ? `last run ${new Date(l.last_run * 1000).toLocaleString()}` : 'not run yet'}
            {l.next_run ? ` · next ${new Date(l.next_run * 1000).toLocaleTimeString()}` : ''}
            {' · by '}{l.created_by}
          </div>
          <div className="card-actions">
            <button className="tab" onClick={() => showRuns(l.id)}>runs</button>
            <button
              className="tab"
              onClick={() => api.loopToggle(l.id, !l.enabled).then(refresh)}
            >
              {l.enabled ? 'pause' : 'resume'}
            </button>
            <button className="no" onClick={() => api.loopDelete(l.id).then(refresh)}>
              delete
            </button>
          </div>
          {runs[l.id] && (
            <div className="runs">
              {runs[l.id].length === 0 && <div className="card-meta">no runs recorded</div>}
              {runs[l.id].map((run) => (
                <div key={run.id} className="run-row">
                  <span className={run.status === 'ok' ? 'good' : 'bad'}>{run.status}</span>
                  <span className="run-time">
                    {new Date(run.started_at * 1000).toLocaleString()}
                  </span>
                  <span className="run-toks">{run.tokens_in}/{run.tokens_out} tok</span>
                  <span className="run-outcome">{run.outcome}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      ))}
    </div>
  )
}

// ---- memory ----

function Memory() {
  const [status, setStatus] = useState(null)
  useEffect(() => {
    api.memoryStatus().then(setStatus).catch(() => setStatus(null))
  }, [])
  if (!status) return <div className="empty">memory engine disabled</div>
  return (
    <div className="list">
      <div className="card">
        <div className="card-title">memory engine</div>
        <table>
          <tbody>
            <tr><td>vault</td><td>{status.vault}</td></tr>
            <tr><td>embedder</td><td>{status.embedder}</td></tr>
            <tr><td>entities</td><td>{status.entities}</td></tr>
            <tr><td>facts (active)</td><td>{status.facts}</td></tr>
            <tr><td>edges (active)</td><td>{status.edges}</td></tr>
            <tr><td>pending consolidation</td><td>{status.pending}</td></tr>
          </tbody>
        </table>
        <div className="card-meta">open the vault folder in Obsidian for the graph view</div>
      </div>
    </div>
  )
}

// ---- settings: gateway, models/tiers, keys, health ----

function Settings() {
  const [config, setConfig] = useState(null)
  const [health, setHealth] = useState(null)
  useEffect(() => {
    api.config().then(setConfig).catch(() => setConfig(null))
    const refresh = () => api.health().then(setHealth).catch(() => {})
    refresh()
    const timer = setInterval(refresh, 5000)
    return () => clearInterval(timer)
  }, [])
  if (!config) return <div className="empty">loading…</div>

  return (
    <div className="list">
      <div className="card">
        <div className="card-title">gateway (agentgateway, supervised)</div>
        <table>
          <tbody>
            <tr><td>mode</td><td>{config.gateway.mode}</td></tr>
            <tr><td>version</td><td>{config.gateway.version}</td></tr>
            <tr><td>llm port</td><td>{config.gateway.llm_port}</td></tr>
            <tr>
              <td>health</td>
              <td className={health?.gateway_healthy ? 'good' : 'bad'}>
                {health?.gateway_healthy ? '✓ healthy' : '✗ unreachable'}
              </td>
            </tr>
          </tbody>
        </table>
        <div className="card-meta">
          admin UI: <a href="http://localhost:15000/ui" target="_blank" rel="noreferrer">localhost:15000/ui</a>
        </div>
      </div>

      <div className="section-label">model tiers → provider routing</div>
      <div className="card-meta">
        the harness sends a tier alias; agentgateway routes it to the provider
        models below (top = primary, rest = failover). Default tier:{' '}
        <b>{config.default_tier}</b>. Edit tiers in ~/.revenant/config.toml.
      </div>
      {Object.entries(config.tiers).map(([name, tier]) => (
        <div key={name} className="card">
          <div className="card-title">
            {name}
            {tier.failover && <span className="pill sm" style={{ background: '#60a5fa', marginLeft: 8 }}>failover</span>}
          </div>
          {tier.targets.map((t, i) => (
            <div key={i} className="tool-row">
              <b>{t.provider}</b>
              <span>{t.model}</span>
              <span className={t.key_present ? 'good' : 'bad'}>
                {t.api_key_env ? (t.key_present ? `✓ ${t.api_key_env}` : `✗ ${t.api_key_env} missing`) : 'no key needed'}
              </span>
            </div>
          ))}
        </div>
      ))}

      <div className="section-label">API keys</div>
      <div className="card">
        <div className="card-meta">
          keys live only in ~/.revenant/secrets.env and are injected into the
          gateway process — never stored by revenant, never sent to the browser.
        </div>
        {config.keys_present.length === 0 && <div className="card-meta">none set</div>}
        {config.keys_present.map((k) => (
          <div key={k} className="tool-row">
            <b className="good">✓ {k}</b>
            <span>set</span>
          </div>
        ))}
      </div>

      <div className="card-meta">embedder: {config.embedder}</div>
    </div>
  )
}
