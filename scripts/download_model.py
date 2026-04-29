#!/usr/bin/env -S uv run --script
# /// script
# dependencies = ["huggingface_hub"]
# ///
"""Download Qwen3-Embedding-0.6B model from Hugging Face."""
from huggingface_hub import snapshot_download

model_id = "Qwen/Qwen3-Embedding-0.6B"
cache_dir = None  # Uses default HF cache: ~/.cache/huggingface/hub

print(f"Downloading {model_id}...")
snapshot_download(repo_id=model_id, cache_dir=cache_dir)
print(f"Model downloaded to HF cache")
