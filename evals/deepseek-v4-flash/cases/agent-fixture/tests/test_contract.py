import unittest
from unittest.mock import Mock
from contract import Driver, Runtime
class TestContract(unittest.TestCase):
    def test_driver_delegates(self):
        runtime=Runtime(); runtime.set_value=Mock(wraps=runtime.set_value); Driver(runtime).update(7)
        runtime.set_value.assert_called_once_with(7); self.assertEqual(runtime.value,7)
