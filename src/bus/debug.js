const MESSAGE_RETENTION_MS = 5 * 60 * 1000;

const messageLog = [];

function init(bus) {
  // Subscribe to bus activity
  bus.subscribe((activity) => {
    messageLog.push(activity);

    // Purge old messages
    const cutoff = Date.now() - MESSAGE_RETENTION_MS;
    while (messageLog.length > 0 && messageLog[0].timestamp < cutoff) {
      messageLog.shift();
    }
  });
}

function getDebugInfo() {
  return {
    message_count: messageLog.length,
    messages: messageLog.slice(-100),
    retention_ms: MESSAGE_RETENTION_MS,
  };
}

module.exports = { init, getDebugInfo };
