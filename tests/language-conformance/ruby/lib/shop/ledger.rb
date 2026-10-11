require_relative "helper"

module Shop
  module Ledger
    def self.clamp(value)
      [value, 0].max
    end

    # Sum the values, scaled and clamped.
    def self.total(values)
      double = ->(x) { x * 2 }
      values.sum { |value| clamp(Helper.scale(double.call(value), 2)) }
    end
  end

  class Account
    def initialize
      @balance = 0
    end

    def deposit(amount)
      @balance += amount if self.validate(amount)
    end

    def validate(amount)
      amount.positive?
    end
  end
end
