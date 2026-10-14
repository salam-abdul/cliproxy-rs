export async function enableRoutedRemoteControl(query, sessionId) {
  if (typeof query.enableRemoteControl !== 'function') {
    throw new Error('Zed routed Claude: installed SDK is missing enableRemoteControl');
  }
  const result = await query.enableRemoteControl(true, 'Zed Claude');
  if (typeof result?.session_url !== 'string' || !result.session_url.startsWith('https://claude.ai/code/')) {
    throw new Error('Zed routed Claude: Remote Control did not return a session URL');
  }
  console.error(JSON.stringify({event:'claude-route-acp-remote-control',sessionId,url:result.session_url}));
  return result;
}
