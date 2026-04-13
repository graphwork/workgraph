# Local Models for TerminalBench: RTX 6000 Ada 48GB + 96GB RAM

## Hardware Profile
- **GPU:** NVIDIA RTX 6000 Ada Generation — 48GB GDDR6 VRAM, 18,176 CUDA cores
- **RAM:** 96GB system DDR5
- **Target:** Run TerminalBench coding tasks locally, eliminating API costs and rate limits

---

## 1. Candidate Models Evaluated

### Tier 1: Recommended — MoE Coding Specialists

| Model | Total Params | Active Params | Quant | VRAM (est.) | SWE-bench Verified | HumanEval | tok/s (RTX 6000 Ada) |
|-------|-------------|---------------|-------|-------------|-------------------|-----------|----------------------|
| **Qwen3-Coder-Next** | 80B | 3B | Q4_K_M | ~45GB | **74.2%** | ~93% | **20-30+** |
| **Qwen3-Coder-Next** | 80B | 3B | Q8_0 | ~85GB (offload) | 74.2% | ~93% | ~15 (w/ CPU offload) |
| **Qwen3-Coder-30B** | 30B | 3B | Q8_0 | ~32GB | ~65% | ~90% | **30-40+** |
| **Qwen3-Coder-30B** | 30B | 3B | Q4_K_M | ~18GB | ~63% | ~88% | **50+** |

### Tier 2: Strong Alternatives

| Model | Total Params | Active Params | Quant | VRAM (est.) | SWE-bench Verified | HumanEval | tok/s (RTX 6000 Ada) |
|-------|-------------|---------------|-------|-------------|-------------------|-----------|----------------------|
| **Devstral Small 2** | 24B | 24B (dense) | Q8_0 | ~25GB | **68.0%** | ~88% | **25-35** |
| **Qwen2.5-Coder-32B** | 32B | 32B (dense) | Q4_K_M | ~20GB | ~65% (est.) | **92.7%** | **35-45** |
| **Qwen2.5-Coder-32B** | 32B | 32B (dense) | FP16 | ~64GB (offload) | ~65% | 92.7% | ~12 (w/ offload) |

### Tier 3: Larger Models with CPU Offload (Speed Tradeoff)

| Model | Total Params | Active Params | Quant | VRAM (est.) | SWE-bench Verified | HumanEval | tok/s (RTX 6000 Ada) |
|-------|-------------|---------------|-------|-------------|-------------------|-----------|----------------------|
| **Llama 3.3 70B** | 70B | 70B (dense) | Q4_K_M | ~39GB | ~55% (est.) | ~85% | **~18** |
| **Qwen3-235B-A22B** | 235B | 22B | IQ3_K | ~110GB (offload) | ~70% | ~92% | **~8-14** (heavy offload) |
| **DeepSeek-V3** | 671B | 37B | Q3/Q4 | requires massive offload | ~70%+ | ~90%+ | **~3-8** (impractical) |

### Models NOT Recommended for 48GB

| Model | Why Not |
|-------|---------|
| **DeepSeek-V3/V3.2** (671B) | Even at Q3, needs ~300GB+ — far exceeds 48GB VRAM + 96GB RAM |
| **Qwen3-Coder** (480B) | Full model needs ~960GB FP16; even Q4 dynamic quant is ~276GB |
| **Devstral 2** (123B) | Too large for single-GPU at any useful quantization |

---

## 2. Rankings: Best Coding Model That Fits

### Winner: **Qwen3-Coder-Next (80B MoE, Q4_K_M)**

This is the clear best choice for the hardware:

- **SWE-bench Verified: 74.2%** — This is frontier-model territory. For reference, GPT-OSS-120B (the API model TB has been testing) has no published SWE-bench score, but Qwen3-Coder-Next beats most closed models.
- **Only 3B active parameters per token** — The MoE architecture means the GPU only processes 3B params per forward pass while having 80B total params for representational capacity.
- **Fits in 48GB at Q4_K_M** — ~45GB VRAM, leaving headroom for KV cache.
- **20-30+ tok/s generation** — Fast enough for interactive multi-turn TB tasks (the research suggests 20+ tok/s when the model fits entirely in VRAM).
- **Native agentic training** — Trained specifically on agent trajectories, tool usage, error recovery, and long-horizon coding. Supports up to 256K context.
- **Tool calling support** — XML-style tool calling (qwen3_coder format) works in llama.cpp, Ollama, and vLLM.

### Runner-up: **Qwen3-Coder-30B (MoE, Q8_0)**

If Qwen3-Coder-Next is too tight at Q4 for 256K context tasks:

- Much smaller footprint (~32GB at Q8_0), leaving massive headroom for context
- Still 3B active params per token
- ~65% SWE-bench — good but noticeably below Next
- Would be the safe fallback if VRAM pressure causes OOM with the 80B

### Third: **Devstral Small 2 (24B dense)**

- 68.0% SWE-bench — strong for its size
- Apache 2.0 licensed
- Only ~25GB at Q8 — runs easily with plenty of VRAM headroom
- Dense model = simpler serving, no MoE quirks
- Mistral's own Vibe CLI uses it for agentic coding

---

## 3. Serving Stack Recommendation

### Primary: **Ollama** (simplest path)

**Why Ollama for this use case:**
- Single-user inference (one TB trial at a time) — Ollama is optimized for this
- Native tool calling support for Qwen3, Llama, Devstral models
- OpenAI-compatible API at `http://localhost:11434/v1`
- Zero configuration: `ollama pull qwen3-coder-next` and go
- **Workgraph already has native `ollama:` provider routing** — model format is `ollama:qwen3-coder-next`

**Setup:**
```bash
# Install ollama
curl -fsSL https://ollama.com/install.sh | sh

# Pull the model
ollama pull qwen3-coder-next        # Q4_K_M by default, ~45GB
# OR for the smaller variant:
ollama pull qwen3-coder:30b-a3b     # ~18GB Q4

# Verify it's running
curl http://localhost:11434/v1/models
```

### Alternative: **vLLM** (if running concurrent trials)

vLLM has 35x higher throughput than llama.cpp under concurrent load. If you want to run `n_concurrent_trials > 1`, vLLM is the better backend:

```bash
pip install vllm
vllm serve Qwen/Qwen3-Coder-Next --quantization awq --max-model-len 32768

# Serves at http://localhost:8000/v1
```

vLLM supports tool calling for Qwen3 and Llama models via JSON chat templates.

### Alternative: **llama.cpp** (maximum control)

For raw performance tuning, llama.cpp gives 15-30% better single-user throughput than Ollama:

```bash
# Build llama.cpp with CUDA
cmake -B build -DGGML_CUDA=ON && cmake --build build

# Run server with GGUF model
./build/bin/llama-server \
  -hf unsloth/Qwen3-Coder-Next-GGUF:Q4_K_XL \
  --ctx-size 32768 -ngl 99 --port 8080

# Serves at http://localhost:8080/v1
```

llama.cpp now has MCP client support (merged March 2026), enabling tool calling directly.

### Comparison

| Backend | Tool Calling | Multi-Concurrent | Setup Complexity | Single-User tok/s | wg Integration |
|---------|-------------|-------------------|-----------------|-------------------|----------------|
| **Ollama** | Yes (native) | Poor | Trivial | Good (~85% of raw) | `ollama:model` |
| **vLLM** | Yes (JSON) | Excellent (35x) | Medium | Good | `vllm:model` or endpoint config |
| **llama.cpp** | Yes (MCP) | Poor | Medium | Best (raw) | `llamacpp:model` or endpoint config |

**Recommendation: Start with Ollama.** Switch to vLLM only if you need concurrent trials.

---

## 4. Workgraph / Harbor Integration

### Path A: Direct Workgraph Native Executor (Recommended for Condition A/G)

Workgraph already supports local providers natively. The provider routing in `src/executor/native/provider.rs` handles:

- `ollama:model-name` → routes to `http://localhost:11434/v1` automatically
- `llamacpp:model-name` → routes to `http://localhost:8080/v1` automatically
- `vllm:model-name` → routes to `http://localhost:8000/v1` automatically
- `local:model-name` → routes to `http://localhost:11434/v1` (generic local)

No API key required for local providers — the code explicitly handles `provider_name == "local"` with a dummy key.

**Config for a local TB run:**

```json
{
    "agents": [
        {
            "import_path": "wg.adapter:ConditionAAgent",
            "model_name": "ollama:qwen3-coder-next",
            "kwargs": {
                "max_turns": 50,
                "temperature": 0.0
            }
        }
    ]
}
```

The adapter's `_normalize_model()` function already handles the `ollama` prefix as a known provider, and `_write_trial_wg_config()` writes it into `config.toml` as `model = "ollama:qwen3-coder-next"`.

### Path B: Harbor's OpenAI-Compatible Endpoint

Harbor auto-detects active backends (ollama, llamacpp, vllm, tabbyapi, sglang, etc.) and injects backend URL and model name as environment variables. If using Harbor directly:

```bash
# Harbor detects the running ollama instance
harbor up ollama

# Or point Harbor at any OpenAI-compatible endpoint
harbor config set llm.base_url http://localhost:11434/v1
```

### Config Changes Needed

1. **In the TB config JSON**, change `model_name` from `"openrouter:openai/gpt-oss-120b"` to `"ollama:qwen3-coder-next"`
2. **No OPENROUTER_API_KEY needed** — local inference has no auth requirement
3. **Consider adjusting `n_concurrent_trials`** — with Ollama (single-user optimized), set to 1. With vLLM, can keep at 4-5.
4. **Consider adjusting timeouts** — local inference is slower on prompt processing for long contexts. May want `timeout_multiplier: 2.0` initially.

---

## 5. Expected Performance

### Qwen3-Coder-Next (Q4_K_M) on RTX 6000 Ada

| Metric | Expected Value |
|--------|---------------|
| Prompt processing | ~1400 tok/s (even at 256K context) |
| Generation speed | 20-30+ tok/s |
| Time per TB turn (est.) | ~5-15 seconds (depending on output length) |
| Full 50-turn trial (est.) | ~5-12 minutes |
| Full 89-task run (serial) | ~8-18 hours |
| VRAM usage | ~45GB (Q4_K_M) |
| Leftover VRAM for KV cache | ~3GB |

### Qwen3-Coder-30B (Q8_0) on RTX 6000 Ada

| Metric | Expected Value |
|--------|---------------|
| Generation speed | 30-40+ tok/s |
| Time per TB turn (est.) | ~3-10 seconds |
| Full 89-task run (serial) | ~5-12 hours |
| VRAM usage | ~32GB |
| Leftover VRAM for KV cache | ~16GB (generous) |

### For Context: API Model Comparison

| Model | tok/s (API) | Rate Limits | Cost per 89-task run |
|-------|------------|-------------|---------------------|
| GPT-OSS-120B (OpenRouter) | ~50-100+ | Yes, throttled | ~$10-30 |
| Nemotron-3-Super (OpenRouter) | ~50-100+ | Yes, throttled | ~$5-15 |
| MiniMax-M2.7 (OpenRouter) | ~50-100+ | Yes (free tier limits) | Free but limited |

---

## 6. Honest Assessment: Local vs API

### Where Local Wins
- **Zero rate limits** — No 429s, no exponential backoff, no waiting. Run 89 tasks serially without interruption.
- **Zero cost** — Electricity only. No API bills.
- **Privacy** — Code never leaves your machine.
- **Reproducibility** — Same model, same hardware, deterministic (temperature=0).
- **Qwen3-Coder-Next is genuinely strong** — 74.2% SWE-bench is competitive with frontier closed models.

### Where Local Loses
- **Speed** — Even at 20-30 tok/s, a local model is 2-5x slower than API models that return at 50-100+ tok/s. An 89-task run that takes ~3-5 hours via API will take ~8-18 hours locally.
- **Context window pressure** — Q4 quantization at 48GB leaves only ~3GB for KV cache. Long TB tasks with many turns may run into context limits. The 30B variant gives much more headroom here.
- **Concurrent trials** — API models handle 5 concurrent trials trivially. Locally with Ollama, you're limited to 1 (or 2-3 with vLLM but slower per trial).
- **Quality ceiling** — Qwen3-Coder-Next is excellent but may not match the raw reasoning depth of Opus-class models on the hardest TB tasks.

### The Bottom Line

**Qwen3-Coder-Next is absolutely good enough for TB tasks.** At 74.2% SWE-bench, it's dramatically better than GPT-OSS-120B (which scored 5.6% pass rate on TB condition A). The model quality is not the bottleneck — GPT-OSS-120B's TB failures were largely about agentic tool use and error recovery, not raw intelligence, and Qwen3-Coder-Next was specifically trained for exactly those capabilities.

**Recommended approach:** Run Qwen3-Coder-Next locally via Ollama as Condition A with `n_concurrent_trials: 1`. Compare against the existing GPT-OSS-120B results. If the pass rate is comparable or better, local inference eliminates the entire API cost/rate-limit problem.

---

## 7. Quick-Start Recipe

```bash
# 1. Install Ollama
curl -fsSL https://ollama.com/install.sh | sh

# 2. Pull the model (takes ~25GB download, ~45GB VRAM when loaded)
ollama pull qwen3-coder-next

# 3. Verify it's working
curl http://localhost:11434/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "qwen3-coder-next", "messages": [{"role": "user", "content": "Write a Python function to reverse a string"}]}'

# 4. Create TB config
cat > terminal-bench/local-qwen3-coder-next-config.json << 'EOF'
{
    "job_name": "local-qwen3-coder-next-condition-A",
    "jobs_dir": "results/local-qwen3-coder-next-condition-A",
    "n_attempts": 1,
    "timeout_multiplier": 2.0,
    "debug": false,
    "n_concurrent_trials": 1,
    "quiet": false,
    "retry": {
        "max_retries": 2,
        "exclude_exceptions": [
            "VerifierTimeoutError",
            "AgentTimeoutError",
            "VerifierOutputParseError",
            "RewardFileNotFoundError",
            "RewardFileEmptyError"
        ]
    },
    "environment": {
        "type": "docker"
    },
    "agents": [
        {
            "import_path": "wg.adapter:ConditionAAgent",
            "model_name": "ollama:qwen3-coder-next",
            "kwargs": {
                "max_turns": 50,
                "temperature": 0.0
            }
        }
    ],
    "datasets": [
        {
            "name": "terminal-bench",
            "version": "2.0"
        }
    ]
}
EOF

# 5. Run it
# (from terminal-bench directory)
# python run_condition_a.py --config local-qwen3-coder-next-config.json
```

**Important**: The Docker container inside TB needs to reach the host's Ollama. Ensure Docker networking allows access to `host.docker.internal:11434` or use `--network host`.

---

## Appendix: VRAM Budget Calculator

Formula: `VRAM = (params_billions × bytes_per_param) + KV_cache + overhead`

| Model | Params | Quant | Bytes/Param | Model Size | KV Cache (32K ctx) | Total |
|-------|--------|-------|-------------|-----------|-------------------|-------|
| Qwen3-Coder-Next 80B MoE | 80B (3B active) | Q4_K_M | ~0.56 | ~45GB | ~0.5GB | ~45.5GB |
| Qwen3-Coder-30B MoE | 30B (3B active) | Q8_0 | ~1.1 | ~32GB | ~0.5GB | ~32.5GB |
| Qwen3-Coder-30B MoE | 30B (3B active) | Q4_K_M | ~0.56 | ~18GB | ~0.5GB | ~18.5GB |
| Devstral Small 2 24B | 24B | Q8_0 | ~1.1 | ~25GB | ~1.5GB | ~26.5GB |
| Qwen2.5-Coder-32B | 32B | Q4_K_M | ~0.56 | ~20GB | ~2GB | ~22GB |
| Llama 3.3 70B | 70B | Q4_K_M | ~0.56 | ~39GB | ~5GB | ~44GB |

Note: MoE models have much smaller KV caches because only the active parameters participate in attention. Dense models at 70B use significantly more KV cache per token.
