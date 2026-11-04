#!/usr/bin/env python3
"""Does the message on stdin carry a good signature from this public key? Runs inside fakevps."""
import sys
import dkim

public = sys.argv[1]
message = sys.stdin.buffer.read()
if b"DKIM-Signature" not in message:
    sys.exit("no DKIM-Signature header")


def dns(name, timeout=5):
    return f"v=DKIM1; k=rsa; p={public}".encode()


sys.exit(0 if dkim.verify(message, dnsfunc=dns) else "the signature does not match this key")
