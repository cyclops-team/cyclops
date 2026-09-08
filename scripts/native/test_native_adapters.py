#!/usr/bin/env python3
"""No vendor process or user application is launched by these tests."""

import subprocess
import unittest
from unittest.mock import Mock

from antigravity_sidecar_probe import dispatch, doorbell_command


class AntigravityTransport(unittest.TestCase):
    def test_uses_documented_conversation_command_without_shell_interpolation(self):
        command = doorbell_command("agentapi", "conversation-1; echo no", "cyc-01234567-89abcdef")
        self.assertEqual(command[:3], ["agentapi", "send-message", "conversation-1; echo no"])
        self.assertIn("cyclops inbox claim cyc-01234567-89abcdef", command[3])

    def test_accepts_current_historical_and_attempt_locators(self):
        for locator in [
            "cyc-01234567-89abcdef",
            "m-0123456789abcdef0123456789abcdef",
            "m-att_ASNFZ4mrTe-BI0VniavN7w",
        ]:
            with self.subTest(locator=locator):
                self.assertIn(locator, doorbell_command("agentapi", "conversation-1", locator)[3])

    def test_refuses_option_or_control_character_conversation_ids(self):
        for conversation in ["--help", "-id", "a\nb", "a\rb", "a\x00b", "a\x7fb", " id "]:
            with self.subTest(conversation=conversation), self.assertRaises(ValueError):
                doorbell_command("agentapi", conversation, "cyc-01234567-89abcdef")

    def test_success_is_transport_only_not_a_claim_or_processing_ack(self):
        run = Mock(return_value=subprocess.CompletedProcess([], 0))
        result = dispatch(["agentapi", "send-message", "conversation-1", "notice"], run)
        self.assertEqual(result["transport"], "api_returned_success")
        self.assertFalse(result["processing_acknowledged"])
        self.assertFalse(result["claimed"])
        self.assertEqual(run.call_count, 1)
        self.assertNotIn("shell", run.call_args.kwargs)

    def test_timeout_never_retries_an_uncertain_submission(self):
        run = Mock(side_effect=subprocess.TimeoutExpired("agentapi", 15))
        self.assertEqual(dispatch(["agentapi"], run)["transport"], "outcome_unknown")
        self.assertEqual(run.call_count, 1)

    def test_refuses_a_multiline_or_forged_claim_locator(self):
        for locator in ["admin", "m-real\nrm -rf", "m-real;false", "cyc-01234567-89ABCDEF", "--help"]:
            with self.assertRaises(ValueError):
                doorbell_command("agentapi", "conversation-1", locator)


if __name__ == "__main__":
    unittest.main()
