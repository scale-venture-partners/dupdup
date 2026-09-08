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


def test_mock_assertion_only():
    mock_client = MagicMock()
    call_service(mock_client)
    mock_client.assert_called_once_with("expected-arg")


def test_unittest_style_assertion_only(self):
    self.assertEqual(compute_something(), 3)


def test_determinism_call_repeated():
    assert stable_hash("hello") == stable_hash("hello")


def test_reassert_after_action():
    assert len(cache.gets) == 2
    trigger_bypass_request()
    assert len(cache.gets) == 2


def _assert_round_trip(model_cls, payload):
    instance = model_cls.model_validate(payload)
    assert instance.model_dump() == payload


def test_uses_local_assertion_helper():
    _assert_round_trip(SomeModel, {"a": 1})


def test_uses_pytest_fail():
    over_limit = compute_over_limit()
    if over_limit:
        pytest.fail(f"{len(over_limit)} item(s) over limit")


def test_uses_pytest_skip():
    pytest.skip("not yet implemented")


def app_with_rate_limit():
    """A fixture-style helper, not a test — its inner route handler just
    happens to be named like one."""

    async def test_endpoint():
        return {"message": "ok"}

    return test_endpoint
