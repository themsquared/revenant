// Thin client for the /v1 control plane. Token lives in localStorage after
// first login; EventSource can't set headers so SSE uses ?token=.

// A token in the URL fragment (from `revenant open`) is consumed once, then
// stripped from the URL so it doesn't linger in history.
;(function adoptFragmentToken() {
  const match = window.location.hash.match(/token=([^&]+)/)
  if (match) {
    localStorage.setItem('revenant_token', decodeURIComponent(match[1]))
    history.replaceState(null, '', window.location.pathname)
  }
})()

export function getToken() {
  return localStorage.getItem('revenant_token') || ''
}

export function setToken(token) {
  localStorage.setItem('revenant_token', token.trim())
}

async function request(method, path, body) {
  const resp = await fetch(path, {
    method,
    headers: {
      Authorization: `Bearer ${getToken()}`,
      ...(body ? { 'content-type': 'application/json' } : {}),
    },
    body: body ? JSON.stringify(body) : undefined,
  })
  if (resp.status === 401) throw new Error('unauthorized')
  if (!resp.ok) throw new Error(`${method} ${path}: ${resp.status}`)
  return resp.json()
}

export const api = {
  health: () => request('GET', '/v1/health'),
  sessions: () => request('GET', '/v1/sessions'),
  createSession: (peer) => request('POST', '/v1/sessions', { peer }),
  messages: (id) => request('GET', `/v1/sessions/${id}/messages`),
  send: (id, text) => request('POST', `/v1/sessions/${id}/messages`, { text }),
  approvals: () => request('GET', '/v1/approvals'),
  decide: (id, approve) =>
    request('POST', `/v1/approvals/${id}/decision`, { approve, resolver: 'web' }),
  spend: (window) => request('GET', `/v1/spend?window=${window}`),
  skills: () => request('GET', '/v1/skills'),
  gateway: () => request('GET', '/v1/gateway/status'),
  memoryStatus: () => request('GET', '/v1/memory/status'),
}

export function eventStream(onEvent) {
  const source = new EventSource(`/v1/events?token=${encodeURIComponent(getToken())}`)
  const forward = (event) => {
    try {
      onEvent(event.type, JSON.parse(event.data))
    } catch {
      /* keep-alive */
    }
  }
  for (const name of [
    'turn_started',
    'turn_delta',
    'turn_completed',
    'turn_failed',
    'tool_started',
    'tool_finished',
    'approval_created',
    'approval_resolved',
    'gateway_status',
  ]) {
    source.addEventListener(name, forward)
  }
  return source
}
