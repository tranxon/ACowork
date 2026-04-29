// Node.js 22+ has built-in WebSocket
const agentId = "com.example.weather";
const uri = `ws://127.0.0.1:19876/api/agents/${agentId}/stream`;

console.log(`Connecting to ${uri}...`);

const ws = new WebSocket(uri);
let msgCount = 0;

ws.onopen = () => {
  console.log("[OPEN] WebSocket connected");
  // Send test message
  const msg = JSON.stringify({ type: "message", content: "hello" });
  console.log(`[SEND] ${msg}`);
  ws.send(msg);
};

ws.onmessage = (event) => {
  msgCount++;
  console.log(`[RECV #${msgCount}] ${event.data}`);
};

ws.onerror = (event) => {
  console.log(`[ERROR] WebSocket error: ${event.message || event.type}`);
};

ws.onclose = (event) => {
  console.log(`[CLOSE] code=${event.code} reason=${event.reason} clean=${event.wasClean}`);
  process.exit(0);
};

// Timeout after 30 seconds
setTimeout(() => {
  console.log(`[TIMEOUT] 30s elapsed, received ${msgCount} messages total`);
  ws.close();
  process.exit(0);
}, 30000);
