# Local Model Serving Setup for TerminalBench

## Hardware Profile
- **Machine:** puppost (Ubuntu 24.04.3 LTS, kernel 6.17.0-20-generic)
- **CPU:** AMD Ryzen AI 7 350 (8 cores / 16 threads, AVX-512)
- **RAM:** 93.6 GiB DDR5
- **GPU:** None (AMD Radeon 860M iGPU, unsupported for inference)
- **Inference:** CPU-only via Ollama

## Installed Models

| Model | Size | Generation (tok/s) | Prompt Eval (tok/s) | Quality |
|-------|------|--------------------|---------------------|---------|
| **qwen3-coder-next:q4_K_M** | 51 GB | 3.07 | 5.00 | Best (74.2% SWE-bench) |
| **qwen3-coder:30b-a3b-q8_0** | 32 GB | 8.35 | 17.46 | Good (~65% SWE-bench) |

**Recommendation:** Use `qwen3-coder:30b-a3b-q8_0` for TB runs — 2.7x faster generation
with good quality. Use `qwen3-coder-next:q4_K_M` only when quality matters more than speed.

## Server Details

- **Binary:** `~/bin/ollama` (v0.20.6)
- **Libraries:** `~/ollama-lib/`
- **Models:** `~/.ollama/models/`
- **Port:** 11435 (not 11434, which runs the old system v0.9.3)
- **API Base URL:** `http://localhost:11435/v1`
- **Ollama Native API:** `http://localhost:11435/api`

## Launch Command

```bash
# Start the local model server
OLLAMA_HOST=127.0.0.1:11435 \
OLLAMA_MODELS=$HOME/.ollama/models \
LD_LIBRARY_PATH=$HOME/ollama-lib:$LD_LIBRARY_PATH \
nohup ~/bin/ollama serve > /tmp/ollama-local.log 2>&1 &

# Verify it's running
curl -s http://localhost:11435/v1/models | python3 -m json.tool
```

Or use the helper script:

```bash
./terminal-bench/start-local-model.sh
```

## Verification Endpoints

```bash
# List models
curl -s http://localhost:11435/v1/models

# Chat completion
curl -s http://localhost:11435/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "qwen3-coder:30b-a3b-q8_0", "messages": [{"role": "user", "content": "Hello"}]}'

# Tool calling
curl -s http://localhost:11435/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen3-coder:30b-a3b-q8_0",
    "messages": [{"role": "user", "content": "List files in /tmp"}],
    "tools": [{"type": "function", "function": {"name": "bash", "description": "Run bash", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}}]
  }'
```

## TB Config (Condition A)

```json
{
    "job_name": "local-qwen3-coder-30b-condition-A",
    "jobs_dir": "results/local-qwen3-coder-30b-condition-A",
    "n_attempts": 1,
    "timeout_multiplier": 3.0,
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
            "model_name": "ollama:qwen3-coder:30b-a3b-q8_0",
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
```

## WG Model Format

For workgraph config, use:
- `ollama:qwen3-coder:30b-a3b-q8_0` (faster, recommended)
- `ollama:qwen3-coder-next:q4_K_M` (best quality)

These route to `http://localhost:11435/v1` automatically via the native ollama provider.

## Performance Estimates for TB Runs

### qwen3-coder:30b-a3b-q8_0 (recommended)
- ~200-token response: ~24 seconds
- 50-turn trial: ~20-40 minutes (varies with context length)
- Full 89-task run (serial): ~30-60 hours

### qwen3-coder-next:q4_K_M (highest quality)
- ~200-token response: ~65 seconds
- 50-turn trial: ~55-110 minutes
- Full 89-task run (serial): ~80-160 hours

## Notes

- **CPU-only inference.** This machine has no NVIDIA GPU. The RTX 6000 Ada
  referenced in research is on a different machine.
- **Port 11435** is used because the system Ollama (v0.9.3) occupies 11434.
  When the system Ollama is updated (requires sudo), both can merge to 11434.
- **Memory:** The 30B model uses ~32GB RAM. The 80B uses ~54GB. Don't load both
  simultaneously — 93GB RAM is not enough for both + OS overhead.
- **Context length:** Default is 4096. For longer contexts, set via Ollama
  modelfile or `num_ctx` parameter in the request.
- **Docker networking:** TB Docker containers need to reach `localhost:11435`.
  Use `--network host` or configure `host.docker.internal`.
