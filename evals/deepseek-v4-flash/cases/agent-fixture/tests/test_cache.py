import unittest
from unittest.mock import patch
from cache import Cache
class TestCache(unittest.TestCase):
    @patch("cache.time.monotonic", side_effect=[10.0, 14.9, 15.0])
    def test_expiration_boundary(self, clock):
        cache=Cache(); cache.put("k","v",5); self.assertEqual(cache.get("k"),"v")
        with self.assertRaises(KeyError): cache.get("k")
