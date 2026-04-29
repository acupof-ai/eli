# 2026-04-29 · Auto Handoff Overflow During Grace

## Context

Auto handoff had a grace period that reused the previous anchor for a couple of
turns after writing a new handoff anchor.

## Root Cause

The error path treated context overflow during grace as a normal grace turn. It
decremented the grace counter and returned without writing a new handoff anchor,
so the next turn reused the same oversized context slice.

## Fix

Context overflow and timeout errors now always write a new auto handoff anchor,
even during grace. This advances the next grace anchor to the previous auto
handoff point, producing a shorter context slice on retry.

## Rule

Never treat provider context overflow as a regular grace turn. Overflow is proof
that the active fallback slice is still too large, so the handoff point must
advance immediately.

