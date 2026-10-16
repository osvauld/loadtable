"""Observed queue/execution clocks for opt-in validation, not worker policy.

No execution deadline runs during queue/target/disk admission. A separate queue
limit is optional (zero = unlimited). Sampling is approximate; durable worker
stage timings remain the authoritative result. Never submits or retries jobs.
"""
import math
import time


class WaitBudget:
    QUEUE = {'queued', 'approved', 'preparing', 'waiting_build', 'waiting_run'}
    EXECUTING = {'building', 'running'}
    TERMINAL = {'passed', 'failed', 'interrupted', 'cancelled'}

    def __init__(self, execution_seconds, queue_seconds=0, clock=time.monotonic):
        if not math.isfinite(execution_seconds) or execution_seconds <= 0:
            raise ValueError('execution timeout must be finite and positive')
        if not math.isfinite(queue_seconds) or queue_seconds < 0:
            raise ValueError('queue timeout must be finite and nonnegative (0 = unlimited)')
        self.execution_limit = execution_seconds
        self.queue_limit = queue_seconds
        self.execution = 0.0
        self.queue = 0.0
        self.clock = clock
        self.previous_at = clock()
        self.previous_phase = 'queued'

    def update(self, phase):
        if phase not in self.QUEUE | self.EXECUTING | self.TERMINAL:
            raise ValueError(f'unknown worker phase: {phase}')
        now = self.clock()
        elapsed = now - self.previous_at
        if elapsed < 0:
            raise ValueError('monotonic clock moved backwards')
        if self.previous_phase in self.EXECUTING:
            self.execution += elapsed
        elif self.previous_phase in self.QUEUE:
            self.queue += elapsed
        self.previous_at, self.previous_phase = now, phase
        if self.execution >= self.execution_limit:
            return f'execution timeout ({self.execution:.1f}s executing, {self.queue:.1f}s queued)'
        if self.queue_limit and self.queue >= self.queue_limit:
            return f'queue timeout ({self.queue:.1f}s queued, {self.execution:.1f}s executing)'
        return None
