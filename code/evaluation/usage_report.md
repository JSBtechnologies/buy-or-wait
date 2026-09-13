# Usage report

Generated from the final full-dataset run that produced `output.csv`. The run starts from an empty cache (`--cold`, PLAN.md §2.11/§3) so every call counted here is a real model invocation, not a cache hit.

## Overview

- Requests in this run: 250
- Model calls: 0
- Input tokens: 0
- Output tokens: 0
- Total tokens: 0
- Cache hit rate: N/A (0 calls)
- Avg tokens per request: 0.0
- Avg cost per request: $0.000000
- Estimated total cost: $0.000000

## Per-model breakdown

| Model | Provider | Calls | Cache hits | Input tokens | Output tokens | Total tokens | Avg tokens/call | Est. cost |
|---|---|---|---|---|---|---|---|---|
| — | — | 0 | 0 (—) | 0 | 0 | 0 | 0.0 | $0.000000 |
| **Overall** | — | 0 | 0 (0.0%) | 0 | 0 | 0 | 0.0 | $0.000000 |

## OCR ingestion (fleet/specs/ocr_vllm_pipeline.md)

- Model: baidu/Unlimited-OCR (self-hosted vLLM, RunPod H100)
- Pages OCR'd live: 17 (cache hits: 0)
- Prompt tokens: 31309
- Completion tokens: 11703
- Total tokens: 43012
- Wall time: 53.31s
- Estimated cost: $0.039835 (assumption: $2.69/hr H100, wall-time billed -- not per-token pricing; verify against the actual RunPod rate)
