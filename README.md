# NIFTY agentic paper trader (Rust / Termux)

Paper-only NIFTY options trading agent in Rust. It starts with ₹10,000 of simulated capital and places **zero live orders** (`LIVE_ORDER_PLACEMENT_IMPLEMENTED = false` in `src/lib.rs`, enforced by `tests/safety.rs`). Live order, portfolio, and margin APIs are deliberately unimplemented in Rust; see [docs/indstocks-api.md](docs/indstocks-api.md) for the authoritative INDmoney / INDstocks v1 API reference covering both read-only market data and post-trial live-trading documentation. The `IndependentStrategy` adapter has no webhook path and defaults to a no-op until you connect your existing strategy implementation.

## What is included

- Hybrid scheduling primitives: market-data events plus five-minute context ticks.
- Gemini structured JSON client using `gemini-3.1-flash-lite` at minimal reasoning effort, with a 10 worker → 2 subleader → 1 Driver hierarchy. Workers share one objective and receive distinct analytical lenses; response rate and concurrent calls are bounded in settings.
- Read-only INDstocks REST market-data configuration targeting `https://api.indstocks.com` (raw `Authorization: <INDSTOCKS_ACCESS_TOKEN>` header with **no** `Bearer` prefix, NIFTY expiries, option chain spot/Greeks, and quotes; see [docs/indstocks-api.md](docs/indstocks-api.md)). WebSocket price streaming (`wss://ws-prices.indstocks.com/api/v1/ws/prices`) is optional/documented, and zero live order endpoints exist in Rust.
- Continuous NSE market-hours paper trading workflow (`09:15–15:30 IST`), deterministic tick-level `stop_loss_price`, `trailing_stop_loss_price`, and `target_price` monitoring, and `15:30 IST` end-of-day square-off with a daily P&L/trades summary (target M2/M3 runtime behavior; live `api.indstocks.com` runs require a valid daily token and live smoke verification).
- Deterministic freshness, position, per-trade capital, daily-loss and short-selling gates.
- SQLite signal/fill journal with duplicate signal IDs ignored.
- Paper broker applies configured spread, slippage, fees and partial fills.
- The agent supplies each entry price, target, initial stop loss and initial trailing stop in option-premium units. Rust only rejects malformed plans, enforces capital and safety limits, and simulates those agent-selected exits; the trailing stop ratchets using the gap selected by the agent.
- Safety tests for `LIVE_ORDER_PLACEMENT_IMPLEMENTED = false`, stale data, duplicate signals, capital limits and incomplete worker cycles.
- An opt-in LLM backtest that builds a synthetic weekly NIFTY option chain from historical spot bars and fills decisions at the next bar's open.

## Termux setup

Install current Termux from a maintained source, then:

```sh
pkg update
pkg install rust clang make pkg-config openssl
cd nifty-paper-agent
git pull
bash scripts/setup-secrets.sh
cargo test
cargo run --release
```

The setup script prompts for the INDstocks access token and Google Gemini API key without displaying them. The backtest needs the Gemini key; it does not use the INDstocks token. To set or replace only the Gemini key, run `bash scripts/update-gemini-key.sh`; to replace only the daily INDstocks token, run `bash scripts/update-indstocks-token.sh`. Both scripts preserve the other `.env` entries. Google may use free-tier prompts and responses to improve its products, so avoid putting private strategy details in requests.

## Synthetic options backtest

From either Windows x64 Native Tools Command Prompt or Termux, run:

```sh
cargo run --release -- backtest --max-decisions 1
```

If `data/yahoo_nifty50_60d_5m.json` is missing, the command downloads Yahoo's NIFTY spot 5-minute chart history (up to 60 days). It can also read Yahoo chart JSON or INDmoney OHLC JSON passed with `--data path`. The response is kept under the ignored `data/` folder and is not pushed to GitHub. Yahoo's chart download route is not a stable documented public API; if it stops working, export bars to JSON from Yahoo or INDmoney and pass that file with `--data`.

Each decision runs the 10 workers, 2 subleaders, and Driver: 13 successful Gemini requests. The default pace is 5 requests per minute and response limits are small, with reasoning set to `minimal`. At that pace, a decision takes at least about 2 minutes 24 seconds. After two paced retries for a primary model's HTTP 503, the client switches the remainder of that run to the configured `fallback_model` (`gemini-3.5-flash-lite`). It also retries fallback 503s up to twice. All attempts stay under the configured request-rate limit; 429 quota errors are not retried or switched. The backtest defaults to one decision to limit free-tier usage. Google applies free quotas per project and model; check active model limits in AI Studio before increasing `requests_per_minute` or running more decisions. Ten decisions use up to 130 successful requests, with extra attempts possible after 503s. The result is written to `data/backtest-result.json`.

The option prices are explicitly synthetic: Black-Scholes premiums, rolling 20-session realized volatility (20% fallback while warming up), 50-point strikes around ATM, 65-unit lot size, and a modeled spread. NIFTY weekly expiry uses Thursday before September 2025 and Tuesday from September 2025. The calendar does not adjust expiries for holidays. It does not reproduce historical implied-volatility skew, open interest, queues, actual option quotes, or Tradetron's engine. It is a model replay for the Rust agent, not a reconstruction of market fills. The current project only simulates long one-lot positions and applies the paper capital, per-trade cap, fees, spread, and slippage. The backtest checks an agent-selected target and protective levels against synthetic option OHLC; if one bar crosses both the target and a stop, it assumes the stop was hit first.

The INDmoney connector currently returns at most 250 candles per OHLC request; its 5-minute Greeks are only recent history. Its live chain can help inspect current IV/spreads but cannot provide the 60-day historical chain. The checked Tradetron connector describes its backtest engine as using historical option prices; existing reports can be used as a separate comparison, but this Rust synthetic replay does not run a Tradetron strategy or charge the Tradetron wallet. Yahoo's `yfinance` reference documents intraday history as limited to the latest 60 days, so a five-minute run is bounded to that window.

## Windows command prompt check

Install Rust with rustup and the Visual Studio C++ Build Tools workload (MSVC linker and Windows SDK). Open the Visual Studio 2022 x64 Native Tools Command Prompt, or initialize the toolchain in Command Prompt with:

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
git clone https://github.com/dineshrameshhere-oss/nifty-paper-agent.git
cd nifty-paper-agent
cargo test
cargo run
```

If `rustc` or `cargo` is not recognized, close and reopen Command Prompt after installing Rust so it reloads the updated PATH. To set the secrets without editing `.env`, run `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\setup-secrets.ps1`. For a daily token refresh, run `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\update-indstocks-token.ps1`.

Put the INDstocks access token and Gemini API key in `.env`, not in source or `settings.toml`. The LLM uses Google's OpenAI-compatible endpoint and model IDs are configured in `[llm]`; the default model is Gemini 3.1 Flash-Lite with minimal reasoning effort. To update the Gemini key on Windows, run `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\update-gemini-key.ps1`.

INDstocks v1 authentication requires the **raw** token in `Authorization: <INDSTOCKS_ACCESS_TOKEN>` (**no** `Bearer` prefix), tokens expire after **24 hours**, and read-only market-data requests do **not** require static IP whitelisting (see [docs/indstocks-api.md](docs/indstocks-api.md)). WebSocket streaming (`wss://ws-prices.indstocks.com/api/v1/ws/prices`) remains optional/configuration-only until live token verification.

## 2-Week Live-Data Paper Trading Workflow

All paper-trading modes enforce `LIVE_ORDER_PLACEMENT_IMPLEMENTED = false` (`src/lib.rs`, tested in `tests/safety.rs`), so **zero live broker orders are placed**.

### CLI Commands

- `cargo run --release` — validates `settings.toml`, opens `data/paper.sqlite`, verifies `LIVE_ORDER_PLACEMENT_IMPLEMENTED == false`, and exits safely without network calls.
- `cargo run --release -- paper --demo` — runs a single synthetic paper step (`NIFTY-DEMO-CE`) without calling INDstocks.
- `cargo run --release -- paper` (add `--offline` to use deterministic offline decisions without consuming Gemini quota) — runs the live-data paper-trading session against INDstocks market data (`settings.toml` `[indstocks]`).
- `cargo run --release -- backtest --max-decisions 1` — runs the synthetic historical options backtest.

### Daily Trial Loop (09:15–15:30 IST)

1. **Pre-market token refresh (before `09:15 IST` each trading day)**:
   - Log in to `https://indstocks.com/app/api-trading/access-tokens` and generate a fresh 24-hour token.
   - Update `.env` while keeping `GEMINI_API_KEY` and other entries intact:
     - **Termux**: `bash scripts/update-indstocks-token.sh`
     - **Windows**: `powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\update-indstocks-token.ps1`
2. **Start the paper session**:
   - Run `cargo run --release -- paper` (or `cargo run --release -- paper --offline` to test without LLM calls).
   - During NSE market hours (`09:15–15:30 IST`), the target M2/M3 session loop polls live INDstocks NIFTY spot & option-chain/quote data, evaluates `stop_loss_price`, `trailing_stop_loss_price`, and `target_price` deterministically on incoming ticks, and runs the agent analysis on the configured cadence (`bar_minutes = 5`). *(Note: live calls to `api.indstocks.com` require a valid daily token and have not been live-executed in this repository without user credentials; continuous session orchestration requires M2/M3 completion.)*
3. **End-of-day auto-stop & daily summary (`15:30 IST`)**:
   - At or after `15:30 IST`, open paper positions are squared off at the closing price, a daily summary (realized P&L and trades executed details) is printed, and the process exits cleanly so you can refresh `INDSTOCKS_ACCESS_TOKEN` before the next trading day.

## Configuration and safety

`settings.toml` stores the ₹10,000 starting capital as 1,000,000 paise and configures `[indstocks]` (`https://api.indstocks.com`, `/market/instruments/expiries`, `/market/option-chain` with `exchange = "NSE"`, `segment = "INDEX"`, `underlying_scrip = "40000001"`, `/market/quotes/full`). Review all `[paper]` risk limits before running. This is a software starter, not a profitable strategy or a guarantee of fill realism: the paper broker approximates spread, slippage, and fees, and does not model exchange queues, full taxes, market impact, or outages.

Keep `.env`, `data/paper.sqlite`, logs, and backups private. Rotate credentials if exposed and never paste access tokens into logs, issue trackers, or source control. Android may suspend Termux in the background; disable battery optimization if you run a continuous `09:15–15:30 IST` paper session on-device, and expect disconnects. Resume by checking data freshness; stale input must not be traded.

## Post-Trial Live Readiness & Static-IP Migration (Future)

Do **not** migrate to a static-IP VM for the 2-week paper-trading trial: INDstocks read-only market-data APIs do not require static IP whitelisting. Static IP whitelisting (`NSE/INVG/67858`, Primary/Secondary IPv4 or IPv6, max 1 change per calendar week) is required only if you later enable live order placement.

Before any post-trial transition to live trading, review the full live API contracts and readiness checklist in [docs/indstocks-api.md](docs/indstocks-api.md) — in particular:
- `LIVE_ORDER_PLACEMENT_IMPLEMENTED` remains `false` in `src/lib.rs` (zero live order/account Rust code exists in this repository).
- INDstocks' smart-order `is_tsl` / `tsl_step_size` fields are **not yet live** (silently ignored), so client-side trailing stop-loss management remains required.
- Live order/margin endpoints include non-standard quirks (`GET /order`, `GET /order/trades`, and `GET /margin` use `GET` with JSON bodies; `GET /order` returns HTTP `512` on missing orders; no dedicated `/positions/exit` endpoint exists).
- Static IP availability and cost on an Azure VM (or similar host) depend on the plan/region; keep credentials in a secret store, restrict inbound firewall access, and test reconnects before any live deployment.

## Android / Termux limits

Termux is a Linux-like environment rather than a conventional server. Rust and SQLite should be built natively in Termux; cross-compiling on Windows is not a substitute for validating on-device TLS, SQLite bundled C compilation, WebSocket TLS roots, battery behavior and reconnects. Dependency versions may need minor adjustment for a specific Termux/NDK toolchain. The test suite has been run on Windows/MSVC; the first native Termux build still needs to be checked on your Android device.
