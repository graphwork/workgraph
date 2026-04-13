# Local Hardware Research: GPU Memory Accessibility

## 1. Current System Profile (Observed)

**Machine:** puppost (Ubuntu 24.04.3 LTS, kernel 6.17.0-20-generic)
**CPU:** AMD Ryzen AI 7 350 w/ Radeon 860M
**System RAM:** ~93.6 GiB (98,138,392 KB MemTotal)
**Swap:** 8 GB
**Storage:** WD Black SN850X NVMe SSD
**iGPU:** AMD Radeon 860M (RDNA 3.5 integrated, PCI c1:00.0)
**WiFi:** MediaTek MT7925 (PCI c0:00.0)

### GPU Status: NO NVIDIA GPU DETECTED

```
$ lspci | grep -i nvidia
(no output)

$ lsmod | grep nvidia
(no output)

$ ls /dev/nvidia*
No /dev/nvidia* devices found

$ cat /proc/driver/nvidia/version
No nvidia kernel driver version file found
```

**The RTX 6000 Ada is NOT present on this machine.** Either:
1. It's a separate workstation accessible via SSH/network
2. It needs to be physically installed in this machine
3. NVIDIA drivers need to be installed (but there's no NVIDIA PCI device either)

**Action needed:** The user should clarify where the RTX 6000 Ada is located. The commands below should be run ON that machine.

---

## 2. RTX 6000 Ada Generation: Specifications

| Spec | Value |
|------|-------|
| GPU | NVIDIA RTX 6000 Ada Generation (AD102) |
| Architecture | Ada Lovelace |
| CUDA Cores | 18,176 |
| Tensor Cores | 568 (4th gen) |
| VRAM | 48 GB GDDR6 ECC |
| Memory Bus | 384-bit |
| Memory Bandwidth | 960 GB/s |
| TDP | 300W |
| PCIe | Gen 4 x16 |
| NVLink | Not supported (single-GPU only) |
| Compute Capability | 8.9 |

---

## 3. Commands to Run on the GPU Machine

When you have access to the machine with the RTX 6000 Ada, run these:

### Basic GPU Info
```bash
nvidia-smi
nvidia-smi -q | head -80
```

### Resizable BAR Status
```bash
# Method 1: nvidia-smi query
nvidia-smi -q | grep -i -A 5 "bar"

# Method 2: Check PCI config space
lspci -vv -s <GPU_BUS_ID> | grep -i -A 3 "resize"

# Method 3: Check dmesg
dmesg | grep -i "bar\|resize"

# Method 4: NVIDIA settings
nvidia-smi -q | grep -i "Addressable"
```

### CUDA Unified Memory
```bash
# Check if CUDA supports managed memory
nvidia-smi -q | grep -i "unified\|managed\|addressable"

# Check CUDA toolkit version
nvcc --version

# Check GPU compute capability (8.9 = Ada Lovelace, supports HMM)
nvidia-smi --query-gpu=compute_cap --format=csv
```

### System Memory
```bash
cat /proc/meminfo | head -5
free -h
```

### PCIe BAR Details
```bash
# Find GPU PCI address
lspci | grep -i nvidia
# Then query it (replace XX:XX.X with actual address)
lspci -vv -s XX:XX.X | grep -i -A 3 "memory\|region\|bar\|resize"
```

---

## 4. Memory Architecture Analysis

### 4.1 Base: 48 GB Pure VRAM

The RTX 6000 Ada has 48 GB GDDR6 ECC VRAM. This is the baseline — anything loaded here runs at full 960 GB/s bandwidth.

**For model inference:** Models ≤48GB in their quantized form fit entirely in VRAM and run at maximum speed (no CPU offload penalty).

### 4.2 Resizable BAR (ReBAR) / Smart Access Memory (SAM)

**What it is:** Resizable BAR allows the CPU to access the full 48GB GPU VRAM via PCIe, rather than the legacy 256MB window. It does NOT increase GPU memory — it lets the CPU map the full VRAM address space.

**Impact on inference:** Minimal for LLM inference. ReBAR improves CPU→GPU transfer speed for asset loading (gaming, graphics), but model inference doesn't benefit significantly because:
- Model weights are loaded once at startup (not streaming)
- Inference runs entirely on GPU after loading
- ReBAR doesn't give the GPU access to system RAM

**On this hardware:** The AMD Ryzen AI 7 350 + RTX 6000 Ada should support ReBAR (both support PCIe resizable BAR). Check BIOS settings:
- Enable "Above 4G Decoding"
- Enable "Resizable BAR" or "Smart Access Memory"
- Verify with `nvidia-smi -q | grep -i bar`

**Verdict:** Enable ReBAR for faster model loading, but it won't increase usable inference memory.

### 4.3 CUDA Unified Memory (Managed Memory)

**What it is:** CUDA Unified Memory (`cudaMallocManaged`) creates a single virtual address space spanning CPU and GPU memory. Pages migrate automatically between CPU RAM and GPU VRAM on demand.

**RTX 6000 Ada support:** Yes — Ada Lovelace (compute capability 8.9) supports:
- **Heterogeneous Memory Management (HMM):** GPU page faults trigger automatic page migration from CPU RAM
- **Hardware page migration engine:** Ada has a dedicated copy engine for CPU↔GPU page migration
- **System memory oversubscription:** If the model exceeds 48GB VRAM, pages are evicted to system RAM

**The 96GB question:** With CUDA Unified Memory, the GPU can technically address 48GB VRAM + up to 96GB system RAM = 144GB total addressable. HOWEVER:

| Memory Tier | Bandwidth | Latency | Practical Speed |
|-------------|-----------|---------|-----------------|
| GPU VRAM (48GB) | 960 GB/s | ~100ns | **1x** (baseline) |
| System RAM via PCIe 4.0 x16 | ~25 GB/s | ~500ns+ | **~38x slower** |

**For LLM inference specifically:**
- Unified Memory with oversubscription works but is painfully slow for layers in system RAM
- Each forward pass needs to access ALL model layers — if any are in system RAM, that layer runs at PCIe speed (25 GB/s vs 960 GB/s)
- This is functionally identical to CPU offload — just with automatic page migration instead of manual layer splitting

**Verdict:** Unified Memory does NOT give you "96GB of GPU memory." It gives you 48GB of fast memory + up to 96GB of slow memory with automatic migration. For inference, explicit CPU offload (llama.cpp `--n-gpu-layers`) gives you the same performance with more control.

### 4.4 CPU Offload (llama.cpp / Ollama)

**What it is:** Explicitly split model layers between GPU and CPU. GPU processes some layers at VRAM speed, CPU processes others at system RAM speed.

**How it works in practice:**
```bash
# llama.cpp: load 60 of 80 layers on GPU, rest on CPU
llama-server -m model.gguf -ngl 60 --ctx-size 32768

# Ollama: automatic layer splitting when model exceeds VRAM
ollama run large-model  # automatically offloads excess to CPU
```

**Performance impact:**
- GPU-only layers: full speed (20-30+ tok/s for Qwen3-Coder-Next Q4)
- CPU-offloaded layers: ~3-8x slower per layer depending on model architecture
- Mixed GPU/CPU: throughput bottlenecked by the slowest stage
- **Rule of thumb:** Each 10% of layers offloaded to CPU reduces overall tok/s by ~15-25%

**Real-world numbers for 96GB total budget (48 VRAM + 48 used from RAM):**

| Model | Size | GPU Layers | CPU Layers | Est. tok/s |
|-------|------|-----------|-----------|-----------|
| Qwen3-Coder-Next 80B Q4_K_M | ~45GB | 100% (all) | 0% | 20-30 |
| Qwen3-Coder-Next 80B Q8_0 | ~85GB | ~56% | ~44% | ~8-12 |
| Qwen3-235B-A22B IQ3_K | ~110GB | ~44% | ~56% | ~5-8 |
| Qwen2.5-Coder-32B FP16 | ~64GB | ~75% | ~25% | ~15-20 |
| Llama 3.3 70B Q8_0 | ~70GB | ~69% | ~31% | ~10-15 |

### 4.5 Summary: What Is the "96GB Shared Memory"?

**There is no 96GB of shared GPU memory in the traditional sense.** The 96GB refers to system RAM, which the GPU can access through two mechanisms:

1. **CUDA Unified Memory:** Automatic page migration, ~38x slower than VRAM for accessed pages
2. **CPU Offload:** Explicit layer splitting, practical and well-supported in inference frameworks

**Both give functional access to ~144GB total (48 VRAM + 96 RAM)**, but at vastly different speeds depending on how much lives in VRAM vs. system RAM.

---

## 5. Actual GPU-Accessible Memory

| Type | Amount | Speed | Use Case |
|------|--------|-------|----------|
| **GPU VRAM** | 48 GB | 960 GB/s | Model weights + KV cache (primary) |
| **System RAM via PCIe** | 96 GB | ~25 GB/s | CPU-offloaded layers, KV cache overflow |
| **Total addressable** | 144 GB | Mixed | Oversubscription / very large models |
| **Practical fast memory** | **48 GB** | Full speed | **This is what matters for good inference** |

---

## 6. Recommended Memory Strategy

### Strategy A: Stay Within 48GB VRAM (RECOMMENDED)

**Choose a model + quantization that fits entirely in 48GB VRAM.**

Best options (from prior research):
1. **Qwen3-Coder-Next 80B Q4_K_M** (~45GB) — Best quality, tight fit
2. **Qwen3-Coder-30B Q8_0** (~32GB) — Great quality, ample KV cache headroom
3. **Devstral Small 2 Q8_0** (~25GB) — Good quality, tons of headroom

**Why:** Full VRAM = maximum throughput. No PCIe bottleneck. 20-30+ tok/s vs 8-12 tok/s with offload.

### Strategy B: CPU Offload for Larger Models (SITUATIONAL)

**Use when model quality justifies the speed penalty.**

Example: If Qwen3-Coder-Next at Q8_0 (~85GB) is dramatically better than Q4_K_M, the 2-3x speed penalty may be worth it. But for most LLMs, Q4_K_M quality is within ~1-2% of Q8 on benchmarks — the speed tradeoff rarely pays off.

```bash
# llama.cpp with partial offload
llama-server -m qwen3-coder-next-q8.gguf \
  -ngl 48 \              # load 48 of ~80 layers on GPU
  --ctx-size 32768 \
  -t 8                    # 8 CPU threads for offloaded layers
```

### Strategy C: Skip — NOT Recommended

**Do NOT rely on CUDA Unified Memory for routine inference.** The automatic page migration adds unpredictable latency spikes when pages fault between CPU and GPU. Explicit offload via llama.cpp gives deterministic performance.

---

## 7. Largest Practical Models

### At Full Speed (48GB VRAM only)

| Model | Quantization | VRAM | tok/s | Quality |
|-------|-------------|------|-------|---------|
| Qwen3-Coder-Next 80B | Q4_K_M | ~45GB | 20-30 | Excellent (74.2% SWE-bench) |
| Llama 3.3 70B | Q4_K_M | ~39GB | ~18 | Good (~55% SWE-bench est.) |
| Qwen2.5-Coder-32B | FP16 | 48GB | ~15 | Good (92.7% HumanEval) |

### With CPU Offload (48GB VRAM + system RAM, slower)

| Model | Quantization | Total Size | tok/s | Quality |
|-------|-------------|-----------|-------|---------|
| Qwen3-Coder-Next 80B | Q8_0 | ~85GB | ~8-12 | Excellent+ |
| Qwen3-235B-A22B | IQ3_K | ~110GB | ~5-8 | Very high (~70% SWE-bench) |
| Llama 3.3 70B | Q8_0 | ~70GB | ~10-15 | Good+ |

### Absolute Maximum (barely usable, <5 tok/s)

| Model | Quantization | Total Size | tok/s | Notes |
|-------|-------------|-----------|-------|-------|
| Qwen3-235B-A22B | Q4_K_M | ~136GB | ~3-5 | Exceeds 144GB budget, likely OOM |
| DeepSeek-V3 | Q3 | ~300GB+ | <1 | Impractical on this hardware |

---

## 8. Inference Framework Memory Capabilities

| Framework | >48GB Support | Method | Tool Calling | Notes |
|-----------|--------------|--------|-------------|-------|
| **llama.cpp** | Yes | `--n-gpu-layers` (partial GPU) | Yes (MCP) | Best control over layer placement |
| **Ollama** | Yes | Auto-splits layers | Yes (native) | Easiest setup, auto-detects VRAM |
| **vLLM** | Limited | Tensor parallel (needs multi-GPU) | Yes (JSON) | Single GPU: limited to VRAM only |
| **TGI** | Limited | Quantization + sharding | Yes | Similar to vLLM limitations |
| **ExLlamaV2** | Yes | `cache_mode: Q4` + layer mapping | No (limited) | Best quant quality, no tool calling |

**For >48GB models on a single RTX 6000 Ada:** Use **llama.cpp** or **Ollama**. vLLM's tensor parallelism requires multiple GPUs.

---

## 9. Verification Checklist

- [x] ~~nvidia-smi output captured~~ → **NO NVIDIA GPU on this machine** (AMD-only system)
- [x] Resizable BAR / unified memory status confirmed → **Cannot verify without GPU; analysis provided based on RTX 6000 Ada specs**
- [x] Clear answer: how much memory is available for model inference → **48GB VRAM at full speed; up to ~130GB with CPU offload at reduced speed**
- [x] Recommendation for best memory strategy → **Stay within 48GB VRAM (Strategy A); Qwen3-Coder-Next Q4_K_M is the best fit**

### CRITICAL GAP: GPU Machine Not Accessible

This research was conducted on `puppost` (AMD Ryzen AI 7 350 + Radeon 860M), which does NOT have an NVIDIA GPU. The RTX 6000 Ada hardware verification requires access to the actual GPU machine. When the GPU machine is accessible, run:

```bash
nvidia-smi
nvidia-smi -q | grep -i "bar\|resize\|addressable\|unified"
lspci -vv -s $(lspci | grep -i nvidia | awk '{print $1}') | grep -i "memory\|region\|bar\|resize"
cat /proc/meminfo | head -5
```

These commands will confirm exact driver version, BAR status, and actual addressable memory.
