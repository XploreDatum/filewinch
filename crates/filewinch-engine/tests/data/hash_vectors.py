#!/usr/bin/env python3
"""Independent reference values for the engine's hash tests (src/hash_tests.rs).

QuickXorHash is a direct port of Microsoft's published C# reference (QuickXorHash.cs, three
64-bit cells, the last holding 32 bits), deliberately structured differently from the Rust
implementation (a byte-wise 160-bit circular register). Dropbox content_hash follows Dropbox's
published description (SHA-256 of the concatenated SHA-256 of each 4 MiB block).

Run: python3 crates/filewinch-engine/tests/data/hash_vectors.py
"""
import base64
import hashlib

WIDTH, SHIFT, LAST_CELL_BITS = 160, 11, 32
MASK = (1 << 64) - 1


def quickxor(data: bytes) -> bytes:
    cells = [0] * ((WIDTH - 1) // 64 + 1)
    shift_so_far = 0
    length = 0
    # Feed in chunks of 1000 bytes to exercise the multi-call path of the reference.
    for start in range(0, len(data), 1000) if data else []:
        chunk = data[start:start + 1000]
        cb = len(chunk)
        idx = shift_so_far // 64
        off = shift_so_far % 64
        for i in range(min(cb, WIDTH)):
            last = idx == len(cells) - 1
            bits = LAST_CELL_BITS if last else 64
            if off <= bits - 8:
                for j in range(i, cb, WIDTH):
                    cells[idx] = (cells[idx] ^ (chunk[j] << off)) & MASK
            else:
                i2 = 0 if last else idx + 1
                low = bits - off
                x = 0
                for j in range(i, cb, WIDTH):
                    x ^= chunk[j]
                cells[idx] = (cells[idx] ^ (x << off)) & MASK
                cells[i2] = (cells[i2] ^ (x >> low)) & MASK
            off += SHIFT
            while off >= bits:
                idx = 0 if last else idx + 1
                off -= bits
        shift_so_far = (shift_so_far + SHIFT * (cb % WIDTH)) % WIDTH
        length += cb
    out = bytearray(20)
    for i in range(len(cells) - 1):
        out[i * 8:i * 8 + 8] = cells[i].to_bytes(8, "little")
    out[16:20] = cells[-1].to_bytes(8, "little")[:4]
    for i, b in enumerate(length.to_bytes(8, "little")):
        out[12 + i] ^= b
    return bytes(out)


def dropbox(data: bytes) -> str:
    block = 4 * 1024 * 1024
    overall = hashlib.sha256()
    for i in range(0, len(data), block):
        overall.update(hashlib.sha256(data[i:i + block]).digest())
    return overall.hexdigest()


def pattern(n: int) -> bytes:
    """Same deterministic bytes as `pattern()` in hash_tests.rs."""
    return bytes((i * 31 + i // 7) % 251 for i in range(n))


CASES = {
    "empty": b"",
    "hello": b"Hello, World!",
    "pattern_1000": pattern(1000),
    "pattern_4MiB_plus_1": pattern(4 * 1024 * 1024 + 1),
}

for name, data in CASES.items():
    qx = quickxor(data)
    print(f"{name}: quickxor hex={qx.hex()} base64={base64.b64encode(qx).decode()}")
    print(f"{name}: dropbox={dropbox(data)}")
    print(f"{name}: md5={hashlib.md5(data).hexdigest()} sha1={hashlib.sha1(data).hexdigest()} sha256={hashlib.sha256(data).hexdigest()}")
