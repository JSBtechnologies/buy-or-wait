# Usage report

Generated from the final full-dataset run that produced `output.csv`. The run starts from an empty cache (`--cold`, PLAN.md §2.11/§3) so every call counted here is a real model invocation, not a cache hit.

## Overview

- Requests in this run: 250
- Model calls: 17
- Input tokens: 31309
- Output tokens: 11703
- Total tokens: 43012
- Cache hit rate: 0.0% (0/17 calls served from the §2.11 disk cache, 0 tokens/cost)
- Avg tokens per request: 172.0
- Avg cost per request: $0.000157
- Estimated total cost: $0.039343

## Per-model breakdown

| Model | Provider | Calls | Cache hits | Input tokens | Output tokens | Total tokens | Avg tokens/call | Est. cost |
|---|---|---|---|---|---|---|---|---|
| baidu/Unlimited-OCR | self-hosted vLLM (RunPod H100) | 17 | 0 (0.0%) | 31309 | 11703 | 43012 | 2530.1 | $0.039343 |
| **Overall** | — | 17 | 0 (0.0%) | 31309 | 11703 | 43012 | 2530.1 | $0.039343 |

_OCR cost assumption: baidu/Unlimited-OCR (self-hosted vLLM, RunPod H100) is billed by wall time, not per token -- $0.039343 = 52.65s wall time × $2.69/hr, shown above via a per-token rate back-derived to match; verify against the actual RunPod rate._
