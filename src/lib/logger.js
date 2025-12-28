const LOG_LEVEL = process.env.LOG_LEVEL || 'info';
const LOG_LEVELS = { debug: 0, info: 1, warn: 2, error: 3 };

function createLogger(scope) {
  const level = LOG_LEVELS[LOG_LEVEL] ?? LOG_LEVELS.info;

  const format = (lvl, msg, meta) => {
    const ts = new Date().toISOString();
    const base = `[${ts}][${scope}][${lvl.toUpperCase()}] ${msg}`;
    if (meta && Object.keys(meta).length) {
      return `${base} ${JSON.stringify(meta)}`;
    }
    return base;
  };

  return {
    debug: (msg, meta) => level <= LOG_LEVELS.debug && console.log(format('debug', msg, meta)),
    info: (msg, meta) => level <= LOG_LEVELS.info && console.log(format('info', msg, meta)),
    warn: (msg, meta) => level <= LOG_LEVELS.warn && console.warn(format('warn', msg, meta)),
    error: (msg, meta) => level <= LOG_LEVELS.error && console.error(format('error', msg, meta)),
  };
}

module.exports = { createLogger };
