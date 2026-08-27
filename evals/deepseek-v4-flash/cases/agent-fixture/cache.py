import time

class Cache:
    def __init__(self): self._items = {}
    def put(self, key, value, ttl): self._items[key] = (value, time.time() + ttl)
    def get(self, key):
        value, expires = self._items[key]
        if time.monotonic() >= expires:
            del self._items[key]; raise KeyError(key)
        return value
