const bonjourInstance = require('bonjour')();

function advertise(port, props = {}, log = console) {
  const serviceConfig = {
    name: props.name || 'Unified Hi-Fi Control',
    type: 'roonknob',
    protocol: 'tcp',
    port,
    txt: {
      base: props.base || `http://localhost:${port}`,
      api: '1',
      ...props.txt
    }
  };

  log.info('mDNS: Publishing service', {
    name: serviceConfig.name,
    type: `_${serviceConfig.type}._${serviceConfig.protocol}`,
    port: serviceConfig.port,
    txt: serviceConfig.txt
  });

  const service = bonjourInstance.publish(serviceConfig);

  service.on('up', () => {
    log.info('mDNS: Service published successfully', { name: serviceConfig.name });
  });

  service.on('error', (err) => {
    log.error('mDNS: Service error', { error: err.message || err });
  });

  process.on('exit', () => service.stop());
  return service;
}

/**
 * Discover services on the network via mDNS
 * @param {string} serviceType - Service type to discover (e.g., 'hqplayer' for _hqplayer._tcp)
 * @param {object} opts - Options: timeout (ms, default 5000), log (logger)
 * @returns {Promise<Array>} Array of discovered services
 */
function discover(serviceType, opts = {}) {
  const log = opts.log || console;
  const timeout = opts.timeout || 5000;

  return new Promise((resolve) => {
    const services = [];

    log.info('mDNS: Starting discovery', {
      type: `_${serviceType}._tcp`,
      timeout
    });

    const browser = bonjourInstance.find({ type: serviceType });

    browser.on('up', (service) => {
      log.info('mDNS: Service found', {
        name: service.name,
        host: service.host,
        port: service.port,
        addresses: service.addresses
      });

      services.push({
        name: service.name,
        host: service.host,
        port: service.port,
        addresses: service.addresses || [],
        txt: service.txt || {}
      });
    });

    // Stop after timeout and return results
    setTimeout(() => {
      browser.stop();
      log.info('mDNS: Discovery complete', { found: services.length });
      resolve(services);
    }, timeout);
  });
}

module.exports = { advertise, discover };
