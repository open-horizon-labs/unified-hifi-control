#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary="$repo_dir/target/debug/unified-hifi-control"
voice_env="${VOICE_ENV_FILE:-$HOME/.config/open-horizon-labs/voice.env}"
voice_config_dir="${KIZZ_VOICE_CONFIG_DIR:-$HOME/.config/open-horizon-labs/kizz-voice-uhc}"
voice_data_dir="${KIZZ_VOICE_DATA_DIR:-$HOME/.local/share/open-horizon-labs/kizz-voice-uhc}"

if [[ ! -x "$binary" ]]; then
  echo "UHC binary not found: $binary" >&2
  echo "Build it first with: cargo build" >&2
  exit 1
fi

# Kill UHC processes that own or have accepted connections on the Kizz voice
# port. This also catches relative-path launches whose command line omits the
# checkout path, while leaving unrelated processes alone.
uhc_pids() {
  while read -r pid; do
    [[ -n "$pid" ]] || continue
    if ps -p "$pid" -o command= 2>/dev/null | grep -q 'unified-hifi-control'; then
      echo "$pid"
    fi
  done < <(lsof -t -iTCP:8088 2>/dev/null | sort -u)
}

while read -r pid; do
  [[ -n "$pid" ]] || continue
  kill -TERM "$pid" 2>/dev/null || true
done < <(uhc_pids)
sleep 1
while read -r pid; do
  [[ -n "$pid" ]] || continue
  kill -KILL "$pid" 2>/dev/null || true
done < <(uhc_pids)

if [[ -f "$voice_env" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$voice_env"
  set +a
else
  echo "Voice environment file not found: $voice_env" >&2
  exit 1
fi

export KIZZ_STT_PROVIDER="${KIZZ_STT_PROVIDER:-deepgram}"
export ELEVENLABS_STT_MODEL="${ELEVENLABS_STT_MODEL:-scribe_v2_realtime}"
export ASSEMBLYAI_MIN_TURN_SILENCE_MS="${ASSEMBLYAI_MIN_TURN_SILENCE_MS:-1200}"
export ASSEMBLYAI_MAX_TURN_SILENCE_MS="${ASSEMBLYAI_MAX_TURN_SILENCE_MS:-3000}"
export DEEPGRAM_EOT_TIMEOUT_MS="${DEEPGRAM_EOT_TIMEOUT_MS:-3000}"
export UHC_PORT=8088
export UHC_CONFIG_DIR="$voice_config_dir"
export UHC_DATA_DIR="$voice_data_dir"
export KIZZ_MCP_URL="${KIZZ_MCP_URL:-http://127.0.0.1:18088/mcp}"

# The voice process is a sidecar, not a second controller. Isolate its state and
# keep every music adapter disabled so it cannot compete with the existing UHC
# for a Roon extension identity. Codex delegates actions to KIZZ_MCP_URL.
install -d "$UHC_CONFIG_DIR/unified-hifi" "$UHC_DATA_DIR"
printf '%s\n' \
  '{' \
  '  "hide_knobs_page": true,' \
  '  "hide_hqp_page": true,' \
  '  "hide_lms_page": true,' \
  '  "adapters": {' \
  '    "roon": false,' \
  '    "upnp": false,' \
  '    "openhome": false,' \
  '    "lms": false,' \
  '    "hqplayer": false' \
  '  }' \
  '}' > "$UHC_CONFIG_DIR/unified-hifi/app-settings.json"

"$binary" &
server_pid=$!
trap 'kill "$server_pid" 2>/dev/null || true' EXIT INT TERM

for _ in {1..50}; do
  if curl -fsS --max-time 1 http://127.0.0.1:8088/voice/reliability >/dev/null; then
    echo "UHC voice sidecar ready on :8088 (music MCP: $KIZZ_MCP_URL; Deepgram + AssemblyAI + ElevenLabs $ELEVENLABS_STT_MODEL)"
    wait "$server_pid"
    exit $?
  fi
  if ! kill -0 "$server_pid" 2>/dev/null; then
    wait "$server_pid" || true
    echo "UHC exited before becoming ready" >&2
    exit 1
  fi
  sleep 0.2
done

echo "UHC did not become ready on :8088" >&2
exit 1
