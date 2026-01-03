const MESSAGE_RETENTION_MS = 5 * 60 * 1000;

function createBusDebug(bus, { logger } = {}) {
  const log = logger || console;
  const messageLog = [];

  const unsubscribe = bus.subscribe((activity) => {
    messageLog.push(activity);
    const cutoff = Date.now() - MESSAGE_RETENTION_MS;
    while (messageLog.length > 0 && messageLog[0].timestamp < cutoff) {
      messageLog.shift();
    }
  });

  function getDebugInfo() {
    return {
      message_count: messageLog.length,
      messages: messageLog.slice(-100),
      retention_ms: MESSAGE_RETENTION_MS,
    };
  }

  function stop() {
    unsubscribe();
    messageLog.length = 0;
  }

  debugInstance = { getDebugInfo, stop };
  return debugInstance;
}

module.exports = { initDebug, getDebug };
