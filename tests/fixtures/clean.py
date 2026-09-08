import pytest


def test_raises_context_without_a_bare_assert():
    with pytest.raises(ValueError):
        parse_int("not a number")


def test_two_different_assertions():
    result = compute_pair()
    assert result.first == 1
    assert result.second == 2


def test_real_comparison():
    assert compute_something() == 3


def test_no_assertions_but_suppressed():  # dupdup: ignore[NO_ASSERTIONS]
    compute_something()


@pytest.mark.parametrize("n", [1, 2, 3])
def test_parametrized(n):
    assert compute_something(n) == n * 2
