import unittest
from diagnosis import collect_tags
class TestDiagnosis(unittest.TestCase):
    def test_calls_are_independent(self):
        self.assertEqual(collect_tags("a"),["a"]); self.assertEqual(collect_tags("b"),["b"])
