#!/data/data/com.termux/files/usr/bin/bash
# Diagnostic: test your INDstocks token + endpoints before starting the bot
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
[ -f "$root/.env" ] && source "$root/.env"

TOKEN="${INDSTOCKS_ACCESS_TOKEN:-}"
BASE="https://api.indstocks.com"

if [ -z "$TOKEN" ]; then
  echo "ERROR: INDSTOCKS_ACCESS_TOKEN is not set. Run: bash scripts/update-indstocks-token.sh"
  exit 1
fi

echo "=== Testing INDstocks API connectivity ==="
echo ""

echo "1) Expiries endpoint..."
curl -sf -w "\n   HTTP %{http_code} in %{time_total}s\n" \
  -H "Authorization: $TOKEN" \
  "$BASE/market/instruments/expiries?underlying=NIFTY&segment=DERIVATIVE" \
  | head -5
echo ""

echo "2) Option-chain endpoint (nearest expiry, 5 strikes)..."
# Get nearest expiry first
EXPIRY=$(curl -sf -H "Authorization: $TOKEN" \
  "$BASE/market/instruments/expiries?underlying=NIFTY&segment=DERIVATIVE" \
  | python3 -c "import sys,json; d=json.load(sys.stdin).get('data',[]); print(d[0] if d else 'NONE')" 2>/dev/null || echo "UNKNOWN")
echo "   Nearest expiry: $EXPIRY"

curl -sf -w "\n   HTTP %{http_code} in %{time_total}s\n" \
  -H "Authorization: $TOKEN" \
  "$BASE/market/option-chain?exchange=NSE&segment=INDEX&underlying-scrip=40000001&expiry=$EXPIRY&strike_count=5" \
  | python3 -c "import sys,json; d=json.load(sys.stdin); print(json.dumps({'underlying_ltp': d.get('data',{}).get('underlying_ltp'), 'strikes': len(d.get('data',{}).get('strikes',{}))}, indent=2))" 2>/dev/null
echo ""

echo "=== Done. If you see HTTP 401/403, your token is expired — re-run scripts/update-indstocks-token.sh ==="
