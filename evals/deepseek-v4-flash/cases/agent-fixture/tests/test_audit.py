import unittest
import audit, service
class TestAudit(unittest.TestCase):
    def setUp(self): audit.EVENTS.clear(); service.USERS.clear()
    def test_success_only(self):
        self.assertTrue(service.create_user(" Alice ")); self.assertEqual(audit.EVENTS,[{"kind":"user.created","name":"alice"}])
        self.assertFalse(service.create_user("ALICE")); self.assertEqual(len(audit.EVENTS),1)
