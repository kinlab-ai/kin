using System;
using System.Collections.Generic;
using System.Threading.Tasks;

namespace Shop
{
    public class Forms
    {
        public static readonly Func<int, int> Square = value => value * value;

        private int count;

        public string Label { get; set; } = "forms";

        public static IEnumerable<int> CountUp(int limit)
        {
            for (var value = 0; value < limit; value++)
            {
                yield return value;
            }
        }

        public static async Task<string> FetchAsync(string url)
        {
            await Task.Yield();
            return url;
        }

        public static int Parse(string text) => int.Parse(text);

        public static int Parse(long value) => (int)value;

        [Obsolete("Use Label.")]
        public int GetCount() => count;

        public class Inner
        {
            public string Ping() => "pong";
        }
    }
}
