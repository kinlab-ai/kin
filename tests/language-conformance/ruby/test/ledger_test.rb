require "minitest/autorun"
require_relative "../lib/shop/ledger"

class LedgerTest < Minitest::Test
  def test_total
    assert_equal 12, Shop::Ledger.total([1, 2])
  end
end
