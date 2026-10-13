Fixture provenance (all MIT-licensed community repos; fetched with `gh api` on 2026-10-12).

- getmodel.json, getinputandoutputlist.json, getstate.json, getstate_earc.json, getpoweroption.json:
  https://github.com/tmeynell/ha-eversolo  tests/fixtures/{getmodel,getinputandoutputlist,getstate_local_file,getstate_earc,getpoweroption}.json
  Real captures from a DMP-A8 Gen 2 (fw v1.1.50 / v1.1.80), MAC scrubbed to aa:bb:cc:00:00:01.
  getstate_earc.json is documented there as partly derived (a reconstruction of the TV/eARC input).
- description.xml: https://github.com/andrewdunndev/narjo-eversolo-upnp  eversolo_descriptor.xml
  Real UPnP descriptor from a DMP-A6 (SSDP LOCATION http://<ip>:1054/description.xml).
- PLAY volumeData example (inline in protocol.rs tests): https://github.com/tomekceszke/eversolo-sdk
  tests/conftest.py (STATE_DATA). SDK test data, not a raw capture.
