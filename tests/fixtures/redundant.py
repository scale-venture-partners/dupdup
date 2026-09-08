def test_never_fails():
    compute_something()


def test_repeats_itself():
    result = compute_something()
    assert result == 3
    assert result == 3


def test_tautology():
    value = compute_something()
    assert value == value
