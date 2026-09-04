#!/bin/bash
# make-fixture-session.sh ID [MB] — write a legacy-snapshot session file with
# ~MB megabytes of api_messages (tool_result blobs) into the active sessions
# dir, so a bench can `synaps attach --continue ID --create` a session that
# actually carries state (PLAN-phase3 §5.5: an empty session is not what
# Parked buys back).
#
# env: SYNAPS_BASE_DIR (default ~/.synaps-cli), FIXTURE_MODEL (default
# claude-sonnet-4-5). Prints the file path.
set -eu
ID=${1:?usage: make-fixture-session.sh ID [MB]}
MB=${2:-2}
BASE=${SYNAPS_BASE_DIR:-$HOME/.synaps-cli}
MODEL=${FIXTURE_MODEL:-claude-sonnet-4-5}
DIR=$BASE/sessions
mkdir -p "$DIR"; chmod 700 "$DIR"
OUT=$DIR/$ID.json
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
python3 - "$OUT" "$ID" "$MB" "$MODEL" "$NOW" <<'PY'
import json, sys
out, sid, mb, model, now = sys.argv[1], sys.argv[2], float(sys.argv[3]), sys.argv[4], sys.argv[5]
target = int(mb * 1024 * 1024)
blob = ("lorem ipsum tool output line %05d\n" % 0) * 32   # ~1 KiB per result
msgs, size, i = [], 0, 0
while size < target:
    tu = {"role": "assistant", "content": [{"type": "tool_use", "id": f"toolu_{i}", "name": "bash",
          "input": {"command": f"echo {i}"}}]}
    tr = {"role": "user", "content": [{"type": "tool_result", "tool_use_id": f"toolu_{i}",
          "content": blob.replace("00000", "%05d" % i)}]}
    msgs += [tu, tr]
    size += len(json.dumps(tu)) + len(json.dumps(tr))
    i += 1
msgs.insert(0, {"role": "user", "content": "run the fixture"})
msgs.append({"role": "assistant", "content": [{"type": "text", "text": "done"}]})
sess = {
    "id": sid, "title": "memprof fixture", "model": model, "thinking_level": "medium",
    "system_prompt": None, "created_at": now, "updated_at": now,
    "total_input_tokens": 0, "total_output_tokens": 0, "session_cost": 0.0,
    "message_count": len(msgs), "api_messages": msgs,
}
with open(out, "w") as f:
    json.dump(sess, f)
print(out)
PY
chmod 600 "$OUT"
