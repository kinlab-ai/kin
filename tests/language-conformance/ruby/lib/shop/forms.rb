module Shop
  class Forms
    attr_reader :count
    attr_accessor :label

    SQUARE = ->(value) { value * value }

    def initialize
      @count = 0
      @label = "forms"
    end

    def each_value
      return enum_for(:each_value) unless block_given?

      yield @count
    end

    class Inner
      def ping
        "pong"
      end
    end
  end
end
