import unittest
from pagination import page_count
class TestPagination(unittest.TestCase):
    def test_boundaries(self):
        self.assertEqual(page_count(0,10),0); self.assertEqual(page_count(1,10),1); self.assertEqual(page_count(11,10),2)
        with self.assertRaises(ValueError): page_count(1,0)
