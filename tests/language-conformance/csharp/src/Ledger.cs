using Shop.Util;

namespace Shop;

public static class Ledger
{
    static int Clamp(int value) => System.Math.Max(value, 0);

    /// <summary>Sum the values, scaled and clamped.</summary>
    [System.Obsolete("Use a ledger service.")]
    public static int Total(int[] values)
    {
        int Double(int x) => x * 2;
        var result = 0;
        foreach (var value in values)
        {
            result += Clamp(Helper.Scale(Double(value), 2));
        }
        return result;
    }
}

public class Account
{
    private int balance;

    public void Deposit(int amount)
    {
        if (this.Validate(amount))
        {
            balance += amount;
        }
    }

    public bool Validate(int amount) => amount > 0;
}
