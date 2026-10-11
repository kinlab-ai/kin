using Xunit;

namespace Shop.Tests;

public class LedgerTests
{
    [Fact]
    public void TestTotal()
    {
        Assert.Equal(12, Ledger.Total(new[] { 1, 2 }));
    }
}
