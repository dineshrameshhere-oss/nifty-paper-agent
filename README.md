# NIFTY agentic paper trader (Rust / Termux)

Paper-only starter for NIFTY research. It starts with ₹10,000 of simulated capital and has no broker order-placement code. Live order APIs are deliberately absent. The `IndependentStrategy` adapter has no webhook path and defaults to a no-op until you connect your existing strategy implementation.

## What is included

- Hybrid scheduling primitives: market-data events plus five-minute context ticks.
- Groq structured JSON client and a 10 worker → 2 subleader → 1 Driver hierarchy. Workers share one objective and receive distinct analytical lenses; response rate and concurrent calls are bounded in settings.
- Read-only INDstocks REST adapter scaffold, raw WebSocket URL configuration and explicit endpoint/auth/schema TODOs. No requests are sent until enabled.
- Deterministic freshness, position, per-trade capital, daily-loss and short-selling gates.
- SQLite signal/fill journal with duplicate signal IDs ignored.
- Paper broker applies configured spread, slippage, fees and partial fills.
- Safety tests for stale data, duplicate signals, capital limits and incomplete worker cycles.
- An opt-in LLM backtest that builds a synthetic weekly NIFTY option chain from historical spot bars and fills decisions at the next bar's open.

## Termux setup

Install current Termux from a maintained source, then:

```sh
pkg update
pkg install rust clang make pkg-config openssl
cd nifty-paper-agent
git pull
bash scripts/setup-secrets.sh
nano settings.toml
cargo test
cargo run --release
```

The setup script prompts for the INDstocks access token and Groq API key without displaying them. INDstocks calls use the access token; a separate API key is not needed for this adapter. To replace just the daily INDstocks token later, run `bash scripts/update-indstocks-token.sh`; it preserves the other `.env` entries.

## Synthetic options backtest

From either Windows x64 Native Tools Command Prompt or Termux, run:

```sh
cargo run --release -- backtest --max-decisions 10
```

If `data/yahoo_nifty50_60d_5m.json` is missing, the command downloads Yahoo's NIFTY spot 5-minute chart history (up to 60 days). It can also read Yahoo chart JSON or INDmoney OHLC JSON passed with `--data path`. The response is kept under the ignored `data/` folder and is not pushed to GitHub. Yahoo's chart download route is not a stable documented public API; if it stops working, export bars to JSON from Yahoo or INDmoney and pass that file with `--data`.

Each decision runs the 10 workers, 2 subleaders, and Driver. That is 13 Groq requests per decision; the default cap is 10 decisions so an accidental full-history run does not issue thousands of requests. Raise `--max-decisions` only when you want the additional model usage and waiting time. The result is written to `data/backtest-result.json`.

The option prices are explicitly synthetic: Black-Scholes premiums, rolling 20-session realized volatility (20% fallback while warming up), 50-point strikes around ATM, 65-unit lot size, and a modeled spread. NIFTY weekly expiry uses Thursday before September 2025 and Tuesday from September 2025. The calendar does not adjust expiries for holidays. It does not reproduce historical implied-volatility skew, open interest, queues, actual option quotes, or Tradetron's engine. It is a model replay for the Rust agent, not a reconstruction of market fills. The current project only simulates long one-lot positions and applies the paper capital, per-trade cap, fees, spread, and slippage.

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

Put the INDstocks read-only token and Groq key in `.env`, not in source or `settings.toml`. Replace all `SET_GROQ_*_MODEL` values with model IDs currently available to your Groq account. Set `market_data_enabled = true` only after verifying the official INDstocks API version, URL, authentication, quote schema, instrument identifier, WebSocket subscription payload, and rate limits. The current REST scaffold assumes a bearer token and a simple quote JSON object; that assumption is marked for verification and may need adaptation. The WebSocket URL is configuration only: subscription and message decoding remain TODOs until the provider schema is confirmed.

The normal executable currently validates configuration, opens the SQLite journal and exits safely. This is intentional while the provider-specific schema and your existing strategy adapter are unconfigured. The separate `backtest` command replays supplied historical index bars, asks the configured Groq worker hierarchy for paper decisions, and simulates synthetic option fills. Live order placement remains absent.

## Configuration and safety

`settings.toml` stores the ₹10,000 starting capital as 1,000,000 paise. Review all risk limits before running. This is a software starter, not a profitable strategy or a guarantee of fill realism. The paper broker approximates costs; it does not model exchange queues, taxes in full, option contract specifications, market impact, or outages. Current quote validation and the scaffold do not yet implement full NIFTY option-chain/five-minute candle normalization.

Keep `.env`, `data/paper.sqlite`, logs, and backups private. Rotate credentials if exposed. Use read-only market-data credentials. Never paste access tokens into logs, issue trackers, or source control. Android may suspend Termux in the background; disable battery optimization only if you need a continuous paper session, and expect disconnects. Resume by checking data freshness; stale input must not be traded.

## Azure static-IP migration (future)

Do not migrate for current paper testing. When you later move a reviewed paper system to an Azure VM, pin a stable outbound public IP if the provider requires allowlisting, keep market data and strategy credentials in a secret store, restrict inbound firewall access, add health monitoring and restart handling, and test reconnects. Static IP availability and cost depend on the current Azure plan/region. This repository still contains no live order API, so migration does not enable live trading.

## Android / Termux limits

Termux is a Linux-like environment rather than a conventional server. Rust and SQLite should be built natively in Termux; cross-compiling on Windows is not a substitute for validating on-device TLS, SQLite bundled C compilation, WebSocket TLS roots, battery behavior and reconnects. Dependency versions may need minor adjustment for a specific Termux/NDK toolchain. The test suite has been run on Windows/MSVC; the first native Termux build still needs to be checked on your Android device.
