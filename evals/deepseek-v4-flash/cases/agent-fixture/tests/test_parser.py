import unittest
from parser import parse_count, parse_limit
class TestParser(unittest.TestCase):
    def test_behavior(self):
        for fn in (parse_count,parse_limit):
            self.assertEqual(fn(" 12 "),12)
            for bad in (None,"","+1","1.0"):
                with self.assertRaises(ValueError): fn(bad)
