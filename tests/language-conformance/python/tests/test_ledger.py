from shop.ledger import total


def test_total():
    assert total([1, 2]) == 12
