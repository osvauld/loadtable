import unittest
from wait_budget import WaitBudget


class WaitBudgetTests(unittest.TestCase):
    def setUp(self):
        self.now = 0.0

    def clock(self):
        return self.now

    def test_long_admission_does_not_consume_execution_timeout(self):
        budget = WaitBudget(10, clock=self.clock)
        budget.update('waiting_build')
        self.now = 7200
        self.assertIsNone(budget.update('building'))
        self.now += 9
        self.assertIsNone(budget.update('passed'))
        self.assertEqual(budget.queue, 7200)
        self.assertEqual(budget.execution, 9)

    def test_execution_budget_accumulates_across_build_and_run(self):
        budget = WaitBudget(10, clock=self.clock)
        budget.update('building')
        self.now = 6
        budget.update('waiting_run')
        self.now = 606
        budget.update('running')
        self.now = 610
        self.assertIn('execution timeout', budget.update('running'))
        self.assertEqual(budget.execution, 10)
        self.assertEqual(budget.queue, 600)

    def test_optional_queue_limit_is_reported_separately(self):
        budget = WaitBudget(100, 5, clock=self.clock)
        self.now = 5
        self.assertIn('queue timeout', budget.update('preparing'))
        self.assertEqual(budget.execution, 0)

    def test_invalid_limits_and_phases_fail_closed(self):
        for execution, queue in [(0, 0), (-1, 0), (float('nan'), 0), (1, -1), (1, float('inf'))]:
            with self.assertRaises(ValueError):
                WaitBudget(execution, queue)
        budget = WaitBudget(10, clock=self.clock)
        with self.assertRaises(ValueError):
            budget.update('unknown')


if __name__ == '__main__':
    unittest.main()
