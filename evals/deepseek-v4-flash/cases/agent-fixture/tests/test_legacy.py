import unittest
from unittest.mock import patch
import native
class TestLegacy(unittest.TestCase):
    def test_native_path(self):
        with patch("legacy.import_value", side_effect=AssertionError("legacy fallback")):
            self.assertEqual(native.handle({"value":9}),9)
