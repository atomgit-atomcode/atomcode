import unittest
from binary_search import bsearch


class TestBsearch(unittest.TestCase):
    def test_found_middle(self):
        self.assertEqual(bsearch([1, 3, 5, 7, 9], 5), 2)

    def test_found_first(self):
        self.assertEqual(bsearch([1, 3, 5, 7, 9], 1), 0)

    def test_found_last(self):
        self.assertEqual(bsearch([1, 3, 5, 7, 9], 9), 4)

    def test_not_found(self):
        self.assertEqual(bsearch([1, 3, 5, 7, 9], 4), -1)

    def test_empty(self):
        self.assertEqual(bsearch([], 1), -1)

    def test_single_match(self):
        self.assertEqual(bsearch([42], 42), 0)

    def test_single_miss(self):
        self.assertEqual(bsearch([42], 7), -1)


if __name__ == "__main__":
    unittest.main()
