"""Independent system-entropy workaround and deterministic range-adapter oracle."""
import json
import secrets


def sample_int(words, minimum, maximum):
    if minimum >= maximum:
        raise ValueError("empty interval")
    width = maximum - minimum
    if width == 1:
        return minimum
    threshold = (1 << 64) % width
    for _ in range(128):
        word = next(words)
        if word >= threshold:
            return minimum + word % width
    raise ValueError("candidate budget exhausted")


def self_check():
    words = iter([0, 5, (1 << 64) - 1])
    assert sample_int(words, 0, 3) == 2
    assert (next(words) >> 11) / (1 << 53) == 1 - 2**-53
    assert sample_int(iter([]), 7, 8) == 7
    try:
        sample_int(iter([0] * 128), 0, 3)
    except ValueError:
        pass
    else:
        raise AssertionError("rejection must stop")


if __name__ == "__main__":
    self_check()
    print(json.dumps({"shard": secrets.randbelow(4), "fraction": secrets.randbits(53) / (1 << 53),
                      "identifier_hex": secrets.token_bytes(16).hex()}, sort_keys=True))
