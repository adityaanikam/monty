from collections import deque

# Container lookups (`in`, `[]`, `.get`, `.pop`, `.remove`, ...) call user
# `__eq__` methods that may mutate the container being searched. CPython
# restarts dict/set probes, clamps `list.remove`, and raises for deque;
# Monty matches (see issue #729 — these previously panicked the worker).


# === dict: `in` with a clearing __eq__ restarts the probe (issue #729 repro) ===
D = {}


class DictClearer:
    def __hash__(self):
        return 1

    def __eq__(self, other):
        D.clear()
        for i in range(3000):
            D[i] = i
        return False


for j in range(10):
    D[DictClearer()] = j

assert (DictClearer() in D) is False
assert len(D) == 3000


# === dict: `.get` and `.pop` with defaults survive the mutation ===
D2 = {}


class DictClearer2:
    def __hash__(self):
        return 1

    def __eq__(self, other):
        D2.clear()
        for i in range(100):
            D2[i] = i
        return False


for j in range(10):
    D2[DictClearer2()] = j

assert D2.get(DictClearer2(), 'missing') == 'missing'
assert D2.pop(DictClearer2(), 'missing') == 'missing'


# === dict: `[]` raises KeyError after the restarted probe finds nothing ===
D3 = {}


class DictClearer3:
    def __hash__(self):
        return 1

    def __eq__(self, other):
        D3.clear()
        for i in range(100):
            D3[i] = i
        return False


for j in range(10):
    D3[DictClearer3()] = j

# the KeyError message is the missing key's repr, which contains an object
# address, so only the exception type can be asserted
try:
    D3[DictClearer3()]
    assert False, 'expected KeyError'
except KeyError:
    pass


# === dict: assignment restarts the probe, then inserts ===
D4 = {}


class DictClearer4:
    def __hash__(self):
        return 1

    def __eq__(self, other):
        D4.clear()
        for i in range(100):
            D4[i] = i
        return False


for j in range(10):
    D4[DictClearer4()] = j

D4[DictClearer4()] = 'x'
assert len(D4) == 101


# === set: `in`, `discard` and `add` with a clearing __eq__ ===
S = set()


class SetClearer:
    def __hash__(self):
        return 1

    def __eq__(self, other):
        S.clear()
        for i in range(100):
            S.add(i)
        return False


for j in range(10):
    S.add(SetClearer())

assert (SetClearer() in S) is False
assert len(S) == 100
S.discard(SetClearer())
assert len(S) == 100
S.add(SetClearer())
assert len(S) == 101


# === set: `remove` raises KeyError after the restarted probe finds nothing ===
S2 = set()


class SetClearer2:
    def __hash__(self):
        return 1

    def __eq__(self, other):
        S2.clear()
        for i in range(100):
            S2.add(i)
        return False


for j in range(10):
    S2.add(SetClearer2())

# as above, the KeyError message contains an object address
try:
    S2.remove(SetClearer2())
    assert False, 'expected KeyError'
except KeyError:
    pass


# === list: a matching __eq__ that shrinks the list clamps the removal ===
L = [1, 2, 3]


class ListClearTrue:
    def __eq__(self, other):
        L.clear()
        return True


L.remove(ListClearTrue())
assert L == []


# === list: a matching __eq__ that shifts the list removes the shifted slot ===
# CPython deletes position 2 even though the match happened there before the
# shift, so the element that moved into it (4) is what goes
L2 = [1, 2, 3, 4]


class ListShiftTrue:
    def __eq__(self, other):
        if other == 3:
            L2.pop(0)
            return True
        return False


L2.remove(ListShiftTrue())
assert L2 == [2, 3]


# === list: `in` walks the live length, so a clearing __eq__ just ends the walk ===
L3 = [1, 2, 3]


class ListClearFalse:
    def __eq__(self, other):
        L3.clear()
        return False


assert (ListClearFalse() in L3) is False
assert L3 == []


# === deque: mutation from __eq__ raises during `in` / `index` / `count` ===
dq = deque()


class DequeAppender:
    def __eq__(self, other):
        dq.append(99)
        return False


dq.append(DequeAppender())


# `in` goes via a helper so the raise crosses a call boundary — a bare
# `0 in dq` inside `try` is uncatchable due to a separate monty bug where
# comparison opcodes don't sync the frame ip before running user `__eq__`
def dq_contains(x):
    return x in dq


try:
    dq_contains(0)
    assert False, 'expected RuntimeError'
except RuntimeError as exc:
    assert str(exc) == 'deque mutated during iteration'

try:
    dq.index(0)
    assert False, 'expected RuntimeError'
except RuntimeError as exc:
    assert str(exc) == 'deque mutated during iteration'

try:
    dq.count(0)
    assert False, 'expected RuntimeError'
except RuntimeError as exc:
    assert str(exc) == 'deque mutated during iteration'


# === deque: `remove` raises IndexError (CPython quirk), even on a match ===
dq2 = deque()


class DequeAppender2:
    def __init__(self, ret):
        self.ret = ret

    def __eq__(self, other):
        dq2.append(99)
        return self.ret


dq2.append(DequeAppender2(False))

try:
    dq2.remove(0)
    assert False, 'expected IndexError'
except IndexError as exc:
    assert str(exc) == 'deque mutated during iteration'

dq3 = deque()


class DequeAppender3:
    def __eq__(self, other):
        dq3.append(99)
        return True


dq3.append(DequeAppender3())

try:
    dq3.remove(0)
    assert False, 'expected IndexError'
except IndexError as exc:
    assert str(exc) == 'deque mutated during iteration'


# === deque: a matching compare returns before the mutation check ===
dq4 = deque()


class DequeAppender4:
    def __eq__(self, other):
        dq4.append(99)
        return True


dq4.append(DequeAppender4())
assert (0 in dq4) is True
assert dq4.index(0) == 0
