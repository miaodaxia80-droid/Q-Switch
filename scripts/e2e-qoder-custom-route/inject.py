#!/usr/bin/env python3
"""Safely inject E2E fixtures for the Qoder three-protocol regression.

Safety properties (this script must never clobber real user state):

  * Fixed test UUIDs identify the ONLY rows this script touches.
  * Provider/model/mapping settings are MERGED into the existing JSON arrays,
    never replaced wholesale.
  * `snapshot` records the exact original QSwitch/Qoder/manifest blobs;
    `cleanup` restores them (or removes only the fixed test rows when a
    manifest did not exist before). It never deletes an entire settings key
    just because it exists.
  * The API key is a fixed, permission-less placeholder.

Phases:
  snapshot -> save original state
  routes   -> merge one test provider + one test model + hidden qoder provider
  mapping  -> read generated manifest, merge the test carrier mapping
  cleanup  -> restore the snapshot and drop only the test hidden provider
"""
import json
import os
import sqlite3
import sys
import time

QWITCH_DB = os.path.realpath(
    os.environ.get("QSWITCH_E2E_DB", os.path.expanduser("~/.qswitch/cc-switch.db"))
)
QODER_DB = os.path.realpath(
    os.environ.get(
        "QODER_E2E_DB",
        os.path.expanduser("~/Library/Application Support/Qoder/User/globalStorage/state.vscdb"),
    )
)
QODER_BACKUP_DB = f"{QODER_DB}.backup"
MANIFEST = os.environ.get(
    "QSWITCH_E2E_MANIFEST", os.path.expanduser("~/.qswitch/qoder-models.json")
)
SNAPSHOT = os.environ.get("QSWITCH_E2E_SNAPSHOT", "/tmp/qswitch-e2e-snapshot.json")

# Fixed, clearly-marked test UUIDs.
PROVIDER_UUID = "e2e00000-0000-4000-8000-000000000001"
MODEL_UUID = "e2e00000-0000-4000-8000-000000000002"
PROVIDER_SETTING_ID = PROVIDER_UUID          # value in qoder_custom_providers_v1
ROUTE_SETTING_ID = MODEL_UUID                # value in qoder_custom_routes_v1
HIDDEN_PROVIDER_ID = f"qswitch-custom-{PROVIDER_UUID}"
STABLE_ROUTE_ID = f"qswitch_{PROVIDER_UUID}_{MODEL_UUID}"
CARRIER_ID = os.environ.get("QSWITCH_E2E_CARRIER_ID", "model_e2e_carrier")
CARRIER_NAME = os.environ.get("QODER_E2E_CARRIER_NAME", "E2E-Carrier")

PROVIDER_KEY = "qoder_custom_providers_v1"
ROUTES_KEY = "qoder_custom_routes_v1"
MAPPINGS_KEY = "qoder_carrier_mappings_v1"

ROUTE_NAME = "E2E-TriProtocol-Model"
ROUTE_MODEL = "e2e-custom-model"
# Fixed placeholder with no real permissions.
ROUTE_API_KEY = "sk-e2e-placeholder-no-permission"
ROUTE_EFFORT = "high"
# API format under test; override with QSWITCH_E2E_API_FORMAT if desired.
API_FORMAT = os.environ.get("QSWITCH_E2E_API_FORMAT", "openai_chat")
IS_FULL_URL = os.environ.get("QSWITCH_E2E_IS_FULL_URL", "0") == "1"
ROUTE_BASE_URL = os.environ.get("QSWITCH_E2E_BASE_URL", "http://127.0.0.1:9000/v1")


def _read_setting(cur, key):
    row = cur.execute("SELECT value FROM settings WHERE key=?", (key,)).fetchone()
    if not row or row[0] is None:
        return None
    return row[0]


def _load_array(blob):
    if not blob:
        return []
    data = json.loads(blob)
    return data if isinstance(data, list) else []


def _write_setting(cur, key, value):
    cur.execute("INSERT OR REPLACE INTO settings(key, value) VALUES (?, ?)", (key, value))


def _qoder_db_slots():
    return {
        "primary": QODER_DB,
        "backup": QODER_BACKUP_DB,
    }


def _read_qoder_carrier(db_path):
    con = sqlite3.connect(db_path)
    cur = con.cursor()
    row = cur.execute("SELECT value FROM ItemTable WHERE key='aicoding.customModels'").fetchone()
    con.close()
    return row[0] if row else None


def phase_snapshot():
    snap = {"qswitch": {}, "qoder_carriers": {}, "manifest": None}
    con = sqlite3.connect(QWITCH_DB)
    cur = con.cursor()
    for key in (PROVIDER_KEY, ROUTES_KEY, MAPPINGS_KEY):
        snap["qswitch"][key] = _read_setting(cur, key)
    con.close()

    if os.path.exists(MANIFEST):
        with open(MANIFEST, encoding="utf-8") as fh:
            snap["manifest"] = fh.read()

    for slot, db_path in _qoder_db_slots().items():
        exists = os.path.exists(db_path)
        snap["qoder_carriers"][slot] = {
            "exists": exists,
            "value": _read_qoder_carrier(db_path) if exists else None,
        }

    with open(SNAPSHOT, "w", encoding="utf-8") as fh:
        json.dump(snap, fh, ensure_ascii=False)
    print(f"[inject] snapshot saved -> {SNAPSHOT}")


def phase_routes():
    con = sqlite3.connect(QWITCH_DB)
    cur = con.cursor()

    # --- merge test provider (never replace the whole array) ---
    providers = [
        p for p in _load_array(_read_setting(cur, PROVIDER_KEY))
        if p.get("id") != PROVIDER_SETTING_ID
    ]
    providers.append({
        "id": PROVIDER_SETTING_ID,
        "name": "E2E-TriProtocol-Provider",
        "baseUrl": ROUTE_BASE_URL,
        "apiKey": ROUTE_API_KEY,
        "apiFormat": API_FORMAT,
        "isFullUrl": IS_FULL_URL,
        "anthropicVersion": "2023-06-01",
        "enabled": True,
    })
    _write_setting(cur, PROVIDER_KEY, json.dumps(providers, ensure_ascii=False))

    # --- merge test model ---
    routes = [
        r for r in _load_array(_read_setting(cur, ROUTES_KEY))
        if r.get("id") != ROUTE_SETTING_ID
    ]
    routes.append({
        "id": ROUTE_SETTING_ID,
        "providerId": PROVIDER_SETTING_ID,
        "name": ROUTE_NAME,
        "model": ROUTE_MODEL,
        "reasoningEffort": ROUTE_EFFORT,
        "isReasoning": True,
        "maxInputTokens": 200000,
        "enabled": True,
    })
    _write_setting(cur, ROUTES_KEY, json.dumps(routes, ensure_ascii=False))

    # --- hidden qoder provider mirror ---
    settings_config = {
        "base_url": ROUTE_BASE_URL,
        "apiKey": ROUTE_API_KEY,
        "apiFormat": API_FORMAT,
        "isFullUrl": IS_FULL_URL,
        "anthropicVersion": "2023-06-01",
        "modelCatalog": {"models": [{
            "model": ROUTE_MODEL,
            "displayName": ROUTE_NAME,
            "contextWindow": 200000,
            "is_reasoning": True,
            "routeId": STABLE_ROUTE_ID,
            "reasoning_effort": ROUTE_EFFORT,
        }]},
    }
    cur.execute("DELETE FROM providers WHERE id=? AND app_type='qoder'", (HIDDEN_PROVIDER_ID,))
    cur.execute(
        "INSERT INTO providers (id, app_type, name, settings_config, website_url, category, "
        "created_at, sort_index, notes, icon, icon_color, meta, is_current, in_failover_queue) "
        "VALUES (?, 'qoder', ?, ?, NULL, 'custom-route', ?, 999999, NULL, NULL, NULL, '{}', 0, 0)",
        (HIDDEN_PROVIDER_ID, ROUTE_NAME, json.dumps(settings_config), int(time.time() * 1000)),
    )
    con.commit()
    con.close()

    # --- Qoder storage: merge the BYOK carrier into the primary DB and its
    # recovery copy. Qoder can restore the latter on startup, so changing only
    # state.vscdb makes the carrier disappear before the renderer sees it.
    for _, db_path in _qoder_db_slots().items():
        if not os.path.exists(db_path):
            continue
        con = sqlite3.connect(db_path)
        cur = con.cursor()
        row = cur.execute("SELECT value FROM ItemTable WHERE key='aicoding.customModels'").fetchone()
        models = json.loads(row[0]) if row else []
        models = [m for m in models if m.get("id") != CARRIER_ID]
        models.append({
            "id": CARRIER_ID,
            "provider": "deepseek",
            "providerDisplayName": "DeepSeek",
            "model": "e2e-carrier-model",
        "displayName": CARRIER_NAME,
            "description": "E2E regression carrier routed to the test provider",
            "visible": True, "hasApiKey": True,
            "is_vl": False, "is_reasoning": True,
            "max_input_tokens": 1000000,
            "byokTypeKey": "pg",
            "createTime": int(time.time() * 1000), "updateTime": int(time.time() * 1000),
        })
        cur.execute(
            "INSERT OR REPLACE INTO ItemTable(key, value) VALUES ('aicoding.customModels', ?)",
            (json.dumps(models, ensure_ascii=False),),
        )
        con.commit()
        con.close()
    print(
        f"[inject] routes ok (provider={HIDDEN_PROVIDER_ID}, route={STABLE_ROUTE_ID}, "
        f"format={API_FORMAT}, full_url={IS_FULL_URL})"
    )


def phase_mapping():
    with open(MANIFEST, encoding="utf-8") as fh:
        manifest = json.load(fh)
    entry = next(
        (m for m in manifest["models"] if m.get("provider_id") == HIDDEN_PROVIDER_ID),
        None,
    )
    if not entry:
        print(f"[inject] ERROR: test entry missing from {MANIFEST}", file=sys.stderr)
        sys.exit(1)
    route_id = entry["route_id"]

    con = sqlite3.connect(QWITCH_DB)
    cur = con.cursor()
    mappings = [
        m for m in _load_array(_read_setting(cur, MAPPINGS_KEY))
        if m.get("carrierModelId") != CARRIER_ID
    ]
    mappings.append({"carrierModelId": CARRIER_ID, "routeId": route_id})
    _write_setting(cur, MAPPINGS_KEY, json.dumps(mappings, ensure_ascii=False))
    con.commit()
    con.close()
    print(f"[inject] carrier mapping merged: {CARRIER_ID} -> {route_id}")


def _restore_setting(cur, key, original):
    if original is None:
        # It did not exist before: remove only if its array holds nothing but
        # our test data, to avoid deleting unrelated user content.
        current = _load_array(_read_setting(cur, key))
        remaining = [x for x in current if not _is_test_item(key, x)]
        if remaining:
            _write_setting(cur, key, json.dumps(remaining, ensure_ascii=False))
        else:
            cur.execute("DELETE FROM settings WHERE key=?", (key,))
    else:
        _write_setting(cur, key, original)


def _is_test_item(key, item):
    if key == PROVIDER_KEY:
        return item.get("id") == PROVIDER_SETTING_ID
    if key == ROUTES_KEY:
        return item.get("id") == ROUTE_SETTING_ID
    if key == MAPPINGS_KEY:
        return item.get("carrierModelId") == CARRIER_ID
    return False


def phase_cleanup():
    # Prefer restoring the exact snapshot; fall back to targeted removal.
    snap = None
    if os.path.exists(SNAPSHOT):
        with open(SNAPSHOT, encoding="utf-8") as fh:
            snap = json.load(fh)

    con = sqlite3.connect(QWITCH_DB)
    cur = con.cursor()
    for key in (PROVIDER_KEY, ROUTES_KEY, MAPPINGS_KEY):
        original = snap["qswitch"].get(key) if snap else None
        _restore_setting(cur, key, original)
    # Drop only the single hidden test provider row.
    cur.execute("DELETE FROM providers WHERE id=? AND app_type='qoder'", (HIDDEN_PROVIDER_ID,))
    con.commit()
    con.close()

    # Restore Qoder's custom-model catalog in both its primary database and
    # recovery copy. When snapshot failed, do not touch either database.
    if snap is not None:
        carriers = snap.get("qoder_carriers")
        if carriers is None:  # Backward compatibility with snapshots from v1.
            carriers = {
                "primary": {"exists": True, "value": snap.get("qoder_carrier")},
            }
        for slot, original in carriers.items():
            db_path = _qoder_db_slots().get(slot)
            if not db_path:
                continue
            if not original.get("exists"):
                if os.path.exists(db_path):
                    os.remove(db_path)
                continue
            if not os.path.exists(db_path):
                continue
            con = sqlite3.connect(db_path)
            cur = con.cursor()
            original_carrier = original.get("value")
            if original_carrier is None:
                cur.execute("DELETE FROM ItemTable WHERE key='aicoding.customModels'")
            else:
                cur.execute(
                    "INSERT OR REPLACE INTO ItemTable(key, value) VALUES ('aicoding.customModels', ?)",
                    (original_carrier,),
                )
            con.commit()
            con.close()

    # Restore the exact manifest when it existed. If this run created it, keep
    # the user's other routes and remove only the fixed E2E provider entry so
    # a stale test route cannot survive after the hidden provider is deleted.
    if snap is not None:
        original_manifest = snap.get("manifest")
        if original_manifest is not None:
            with open(MANIFEST, "w", encoding="utf-8") as fh:
                fh.write(original_manifest)
        elif os.path.exists(MANIFEST):
            try:
                with open(MANIFEST, encoding="utf-8") as fh:
                    manifest = json.load(fh)
                models = manifest.get("models")
                if isinstance(models, list):
                    manifest["models"] = [
                        item
                        for item in models
                        if item.get("provider_id") != HIDDEN_PROVIDER_ID
                        and item.get("route_id") != STABLE_ROUTE_ID
                    ]
                    with open(MANIFEST, "w", encoding="utf-8") as fh:
                        json.dump(manifest, fh, ensure_ascii=False, indent=2)
                        fh.write("\n")
            except (OSError, ValueError, TypeError):
                # Never replace an unreadable user manifest during cleanup.
                pass

    if os.path.exists(SNAPSHOT):
        os.remove(SNAPSHOT)
    print("[inject] test fixtures cleaned (QSwitch and Qoder snapshots restored)")


if __name__ == "__main__":
    phase = sys.argv[1] if len(sys.argv) > 1 else "routes"
    {
        "snapshot": phase_snapshot,
        "routes": phase_routes,
        "mapping": phase_mapping,
        "cleanup": phase_cleanup,
    }[phase]()
