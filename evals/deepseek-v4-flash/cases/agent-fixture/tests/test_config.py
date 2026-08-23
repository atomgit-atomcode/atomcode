import unittest
from app import configured_region
class TestConfig(unittest.TestCase):
    def test_environment_precedence(self):
        self.assertEqual(configured_region({"region":"cn"},{"APP_REGION":"us"}),"us")
        self.assertEqual(configured_region({"region":"cn"},{"APP_REGION":""}),"cn")
