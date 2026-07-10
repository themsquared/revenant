import React, { useEffect, useRef, useState } from 'react'
import { api, eventStream, getToken, setToken } from './api.js'

const TABS = ['chat', 'approvals', 'skills', 'tools', 'subagents', 'spend', 'memory', 'status']

export default function App() {
  const [authed, setAuthed] = useState(false)
  const [tab, setTab] = useState('chat')
  const [pendingCount, setPendingCount] = useState(0)
  const [banner, setBanner] = useState(null)

  useEffect(() => {
    if (!getToken()) return
    api
      .health()
      .then(() => setAuthed(true))
      .catch(() => setAuthed(false))
  }, [])

  if (!authed) return <Login onAuthed={() => setAuthed(true)} />

  return (
    <div className="shell">
      <header>
        <span className="brand">revenant</span>
        <nav>
          {TABS.map((name) => (
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
        {tab === 'spend' && <Spend />}
        {tab === 'memory' && <Memory />}
        {tab === 'status' && <Status />}
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
  const bottom = useRef(null)

  const refreshSessions = () => api.sessions().then((r) => setSessions(r.sessions))

  useEffect(() => {
    refreshSessions()
    api.createSession('web').then((r) => setSessionId(r.id))
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
        <div className="log">
          {messages.map((m, i) => (
            <div key={i} className={`msg ${m.role}`}>
              {m.role === 'tool' ? `⚙ ${m.text}` : m.text}
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
          <button onClick={send} disabled={streaming}>
            send
          </button>
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
  useEffect(() => {
    api.spend(window).then((r) => setRows(r.by_model))
  }, [window])

  const max = Math.max(1, ...rows.map((r) => r.tokens_in + r.tokens_out))
  return (
    <div className="spend">
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
              style={{ width: `${(r.tokens_in / max) * 100}%` }}
              title={`${r.tokens_in.toLocaleString()} in`}
            />
            <div
              className="bar out"
              style={{ width: `${(r.tokens_out / max) * 100}%` }}
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
        token counts from per-response usage; gateway GenAI metrics land here in a later
        milestone
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

// ---- subagents ----

function Subagents() {
  const [subs, setSubs] = useState([])
  const [live, setLive] = useState([])
  const refresh = () => api.subagents().then((r) => setSubs(r.subagents))
  useEffect(() => {
    refresh()
    const source = eventStream((type, event) => {
      if (type === 'subagent_spawned') {
        setLive((prev) => [
          { ...event, at: Date.now() },
          ...prev.filter((s) => s.child_session !== event.child_session),
        ])
      }
      if (type === 'subagent_finished') {
        setLive((prev) =>
          prev.map((s) =>
            s.child_session === event.child_session ? { ...s, done: true, ok: event.ok } : s
          )
        )
        setTimeout(refresh, 500)
      }
    })
    const timer = setInterval(refresh, 6000)
    return () => {
      source.close()
      clearInterval(timer)
    }
  }, [])

  return (
    <div className="list">
      <div className="card-meta">
        the agent delegates self-contained subtasks to focused child agents
        (cheaper tier, one level deep). Live spawns appear here.
      </div>
      {live
        .filter((s) => !s.done)
        .map((s) => (
          <div key={s.child_session} className="card running">
            <div className="card-title">
              <span className="spinner">◍</span> #{s.child_session} · {s.tier}
              <small> from #{s.parent_session}</small>
            </div>
            <div>{s.task}</div>
          </div>
        ))}
      {subs.length === 0 && live.length === 0 && (
        <div className="empty">
          no subagents yet — ask the agent to "use a subagent to research X"
        </div>
      )}
      {subs.map((s) => (
        <div key={s.id} className="card">
          <div className="card-title">
            #{s.id}
            <small> from #{s.parent_session} · {s.messages} msgs</small>
          </div>
          <div>{s.task}</div>
          <div className="card-meta">
            {new Date(s.created_at * 1000).toLocaleString()}
          </div>
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

// ---- status ----

function Status() {
  const [health, setHealth] = useState(null)
  useEffect(() => {
    const refresh = () => api.health().then(setHealth)
    refresh()
    const timer = setInterval(refresh, 5000)
    return () => clearInterval(timer)
  }, [])
  if (!health) return <div className="empty">loading…</div>
  return (
    <div className="list">
      <div className="card">
        <div className="card-title">daemon</div>
        <table>
          <tbody>
            <tr>
              <td>version</td>
              <td>{health.version}</td>
            </tr>
            <tr>
              <td>gateway</td>
              <td className={health.gateway_healthy ? 'good' : 'bad'}>
                {health.gateway_healthy ? '✓ healthy' : '✗ unreachable'}
              </td>
            </tr>
          </tbody>
        </table>
        <div className="card-meta">
          gateway admin UI: <a href="http://localhost:15000/ui" target="_blank" rel="noreferrer">localhost:15000/ui</a>
        </div>
      </div>
    </div>
  )
}
