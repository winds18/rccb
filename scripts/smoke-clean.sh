#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${RCCB_BIN:-$ROOT_DIR/target/release/rccb}"
TMP_DIR="$(mktemp -d)"
PROJ_DIR="$TMP_DIR/proj"

cleanup() {
  for instance in s1 s2 s3 s4 s5 s6; do
    "$BIN" --project-dir "$PROJ_DIR" stop --instance "$instance" >/dev/null 2>&1 || true
  done
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

mkdir -p "$PROJ_DIR"

echo "[smoke] bin=$BIN"
echo "[smoke] temp_project=$PROJ_DIR"

# 0) init should generate config + native profile templates
"$BIN" --project-dir "$PROJ_DIR" init >/dev/null
test -f "$PROJ_DIR/.rccb/config.example.json"
test -f "$PROJ_DIR/.rccb/providers/codex.example.json"
grep -q '"timeout_s"' "$PROJ_DIR/.rccb/providers/codex.example.json"
grep -q '"quiet"' "$PROJ_DIR/.rccb/providers/codex.example.json"
echo "MODE_INIT_TEMPLATES_OK"

# 1) stub mode
RCCB_EXEC_MODE=stub "$BIN" --project-dir "$PROJ_DIR" start --instance s1 claude codex >/dev/null 2>&1 &
PID1=$!
sleep 1
"$BIN" --project-dir "$PROJ_DIR" ask --instance s1 --provider codex --caller claude --stream "stub stream check" >"$TMP_DIR/s1.ask.log"
"$BIN" --project-dir "$PROJ_DIR" stop --instance s1 >/dev/null
wait "$PID1"
grep -q "CCB_DONE:" "$TMP_DIR/s1.ask.log"
echo "MODE_STUB_OK"

# 2) ccb mode wrapper error path
RCCB_EXEC_MODE=ccb RCCB_CODEX_CMD=/definitely/not/executable "$BIN" --project-dir "$PROJ_DIR" start --instance s2 claude codex >/dev/null 2>&1 &
PID2=$!
sleep 1
set +e
"$BIN" --project-dir "$PROJ_DIR" ask --instance s2 --provider codex --caller claude "ccb wrapper errpath" >"$TMP_DIR/s2.ask.log" 2>&1
EC2=$?
set -e
"$BIN" --project-dir "$PROJ_DIR" stop --instance s2 >/dev/null || true
wait "$PID2" || true
if [ "$EC2" -eq 0 ]; then
  echo "MODE_CCB_ERRPATH_UNEXPECTED_OK"
  exit 1
fi
grep -qi "ask failed" "$TMP_DIR/s2.ask.log"
echo "MODE_CCB_ERRPATH_OK"

# 3) native mode with deterministic command
RCCB_EXEC_MODE=native RCCB_CODEX_NATIVE_CMD=/bin/cat "$BIN" --project-dir "$PROJ_DIR" start --instance s3 claude codex >/dev/null 2>&1 &
PID3=$!
sleep 1
"$BIN" --project-dir "$PROJ_DIR" ask --instance s3 --provider codex --caller claude --stream "native stream check" >"$TMP_DIR/s3.ask.log"
"$BIN" --project-dir "$PROJ_DIR" stop --instance s3 >/dev/null
wait "$PID3"
grep -qi "native stream check" "$TMP_DIR/s3.ask.log"
echo "MODE_NATIVE_OK"

# 4) native mode project-local binding (.rccb/bin/codex)
mkdir -p "$PROJ_DIR/.rccb/bin"
cat > "$PROJ_DIR/.rccb/bin/codex" <<'EOF'
#!/usr/bin/env bash
cat
EOF
chmod +x "$PROJ_DIR/.rccb/bin/codex"
RCCB_EXEC_MODE=native "$BIN" --project-dir "$PROJ_DIR" start --instance s4 claude codex >/dev/null 2>&1 &
PID4=$!
sleep 1
"$BIN" --project-dir "$PROJ_DIR" ask --instance s4 --provider codex --caller claude --stream "native local binding check" >"$TMP_DIR/s4.ask.log"
"$BIN" --project-dir "$PROJ_DIR" stop --instance s4 >/dev/null
wait "$PID4"
grep -qi "native local binding check" "$TMP_DIR/s4.ask.log"
echo "MODE_NATIVE_LOCAL_BINDING_OK"

# 5) native profile binding (.rccb/providers/codex.json)
mkdir -p "$PROJ_DIR/.rccb/providers" "$PROJ_DIR/.rccb/bin"
cat > "$PROJ_DIR/.rccb/bin/codex-prof" <<'EOF'
#!/usr/bin/env bash
echo "ARGS:$*"
echo "ENV:${RCCB_MARK:-}"
cat
EOF
chmod +x "$PROJ_DIR/.rccb/bin/codex-prof"
cat > "$PROJ_DIR/.rccb/providers/codex.json" <<'JSON'
{
  "cmd": "./.rccb/bin/codex-prof",
  "args": ["profile", "{provider}", "{caller}"],
  "no_wrap": false,
  "env": {
    "RCCB_MARK": "{provider}:{caller}:{req_id}"
  }
}
JSON
RCCB_EXEC_MODE=native "$BIN" --project-dir "$PROJ_DIR" start --instance s5 claude codex >/dev/null 2>&1 &
PID5=$!
sleep 1
"$BIN" --project-dir "$PROJ_DIR" ask --instance s5 --provider codex --caller claude --stream "native profile check" >"$TMP_DIR/s5.ask.log"
"$BIN" --project-dir "$PROJ_DIR" stop --instance s5 >/dev/null
wait "$PID5"
grep -q "ARGS:profile codex claude" "$TMP_DIR/s5.ask.log"
grep -q "ENV:codex:claude:" "$TMP_DIR/s5.ask.log"
echo "MODE_NATIVE_PROFILE_OK"

# 6) cancel in-flight request by req_id
cat > "$PROJ_DIR/.rccb/bin/codex-cancel" <<'EOF'
#!/usr/bin/env bash
sleep 5
echo "late reply"
echo "CCB_DONE: ${CCB_REQ_ID}"
EOF
chmod +x "$PROJ_DIR/.rccb/bin/codex-cancel"
RCCB_EXEC_MODE=native RCCB_CODEX_NATIVE_CMD="$PROJ_DIR/.rccb/bin/codex-cancel" "$BIN" --project-dir "$PROJ_DIR" start --instance s6 claude codex >/dev/null 2>&1 &
PID6=$!
sleep 1
set +e
"$BIN" --project-dir "$PROJ_DIR" ask --instance s6 --provider codex --caller claude --req-id cancel-req-1 "please cancel" >"$TMP_DIR/s6.ask.log" 2>&1 &
ASKPID6=$!
sleep 1
"$BIN" --project-dir "$PROJ_DIR" cancel --instance s6 --req-id cancel-req-1 >"$TMP_DIR/s6.cancel.log" 2>&1
wait "$ASKPID6"
EC6=$?
set -e
"$BIN" --project-dir "$PROJ_DIR" stop --instance s6 >/dev/null || true
wait "$PID6" || true
if [ "$EC6" -eq 0 ]; then
  echo "MODE_CANCEL_UNEXPECTED_OK"
  exit 1
fi
grep -qi "cancel requested" "$TMP_DIR/s6.cancel.log"
grep -Eqi "request canceled|exit_code=130" "$TMP_DIR/s6.ask.log"
"$BIN" --project-dir "$PROJ_DIR" tasks --instance s6 --as-json >"$TMP_DIR/s6.tasks.json"
grep -q '"req_id": "cancel-req-1"' "$TMP_DIR/s6.tasks.json"
grep -q '"status": "canceled"' "$TMP_DIR/s6.tasks.json"
echo "MODE_CANCEL_OK"

echo "[smoke] all checks passed and temp files cleaned."
