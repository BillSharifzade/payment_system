import { describe, expect, it } from "vitest";
import {
  decimalToFraction,
  describeRateChange,
  exactDecimal,
  formatMinor,
  formatRate,
  fractionToFixed,
  minorToInput,
  parseRate,
  rateToInput,
  toMinor,
} from "./money";

const NBSP = " ";

describe("toMinor", () => {
  it("parses operator-typed amounts exactly", () => {
    expect(toMinor("1500.50")).toBe(150050);
    expect(toMinor("1 500,50")).toBe(150050);
    expect(toMinor("5")).toBe(500);
    expect(toMinor("5.")).toBe(500);
    expect(toMinor("5.1")).toBe(510);
    expect(toMinor("00.29")).toBe(29); // a float multiply would give 28.999…
    expect(toMinor("150000")).toBe(15_000_000); // somoni, not diram
  });

  it("rejects zero, negatives, too many decimals and junk", () => {
    for (const bad of ["", "0", "0.00", "-5", "1.234", "abc", "1e3", "5,5,5"]) {
      expect(toMinor(bad), bad).toBeNull();
    }
  });

  it("refuses locale group separators instead of guessing", () => {
    // "1,500.50" (en) and "1.500" (de) are ambiguous — never read as 1.50 or 1500.
    expect(toMinor("1,500.50")).toBeNull();
    expect(toMinor("1.500")).toBeNull();
  });

  it("refuses amounts beyond the exactly representable range", () => {
    expect(toMinor("90071992547409.92")).toBeNull();
    expect(toMinor("90071992547409.91")).toBe(Number.MAX_SAFE_INTEGER);
  });
});

describe("minorToInput", () => {
  it("round-trips through toMinor exactly", () => {
    for (const m of [1, 29, 100, 150050, 15_000_000, 123456789]) {
      expect(toMinor(minorToInput(m))).toBe(m);
    }
    expect(minorToInput(150000)).toBe("1500");
    expect(minorToInput(150050)).toBe("1500.50");
    expect(minorToInput(5)).toBe("0.05");
  });
});

describe("formatMinor with pinned locales", () => {
  it("en: comma groups, dot decimal", () => {
    expect(formatMinor(123456, "TJS", "en")).toBe("1,234.56 TJS");
    expect(formatMinor(123456789, "TJS", "en")).toBe("1,234,567.89 TJS");
  });

  it("de: dot groups, comma decimal — never 1.234.56", () => {
    expect(formatMinor(123456, "TJS", "de")).toBe("1.234,56 TJS");
    expect(formatMinor(123456789, "TJS", "de")).toBe("1.234.567,89 TJS");
  });

  it("tr: dot groups, comma decimal", () => {
    expect(formatMinor(123456, "TJS", "tr")).toBe("1.234,56 TJS");
  });

  it("ru: no-break-space groups, comma decimal", () => {
    expect(formatMinor(123456789, "TJS", "ru")).toBe(`1${NBSP}234${NBSP}567,89 TJS`);
  });

  it("small, zero and negative amounts", () => {
    expect(formatMinor(5, "TJS", "en")).toBe("0.05 TJS");
    expect(formatMinor(0, "TJS", "de")).toBe("0,00 TJS");
    expect(formatMinor(-150, "USD", "en")).toBe("-1.50 USD");
    expect(formatMinor(-150, "USD", "ru")).toBe("-1,50 USD");
  });

  it("is exact at the top of the safe range (no float rounding)", () => {
    expect(formatMinor(Number.MAX_SAFE_INTEGER, "TJS", "en")).toBe(
      "90,071,992,547,409.91 TJS",
    );
  });

  it("never mixes the locale's grouping with a foreign decimal mark", () => {
    for (const locale of ["en", "de", "tr", "ru", "id", "fr"]) {
      const s = formatMinor(123456789, "TJS", locale);
      const decimal = new Intl.NumberFormat(locale, { minimumFractionDigits: 1 })
        .formatToParts(1.5)
        .find((p) => p.type === "decimal")!.value;
      expect(s.endsWith(`${decimal}89 TJS`), `${locale}: ${s}`).toBe(true);
      // Exactly one decimal mark, and it is not also used as a group separator.
      expect(s.split(decimal).length - 1, `${locale}: ${s}`).toBe(1);
    }
  });
});

describe("decimalToFraction", () => {
  it("returns the exact reduced fraction", () => {
    expect(decimalToFraction("10.90")).toEqual({ num: 109, den: 10 });
    expect(decimalToFraction("10")).toEqual({ num: 10, den: 1 });
    expect(decimalToFraction(" 0.1 ")).toEqual({ num: 1, den: 10 });
    expect(decimalToFraction("0.12345678")).toEqual({ num: 6172839, den: 50000000 });
  });

  it("rejects non-positive, over-precise, unsafe and malformed input", () => {
    for (const bad of [
      "",
      "0",
      "0.0",
      "-1",
      "1.123456789",
      "abc",
      "1,5",
      "1e3",
      "123456789012.12345678", // numerator beyond 2^53
    ]) {
      expect(decimalToFraction(bad), bad).toBeNull();
    }
  });
});

describe("parseRate (exact fractions)", () => {
  it("accepts decimals and a/b fractions, reduced", () => {
    expect(parseRate("10.90")).toEqual({ num: 109, den: 10 });
    expect(parseRate("1/3")).toEqual({ num: 1, den: 3 });
    expect(parseRate(" 218 / 20 ")).toEqual({ num: 109, den: 10 });
  });

  it("rejects zero, negative and malformed fractions", () => {
    for (const bad of ["0/3", "3/0", "-1/3", "1/3/4", "1.5/2", "/3", "1/"]) {
      expect(parseRate(bad), bad).toBeNull();
    }
  });
});

describe("editing a stored rate never loses precision", () => {
  it("terminating rates edit as their exact decimal", () => {
    expect(rateToInput({ num: 109, den: 10 })).toBe("10.9");
    expect(rateToInput({ num: 6172839, den: 50000000 })).toBe("0.12345678");
    expect(rateToInput({ num: 11, den: 1 })).toBe("11");
  });

  it("1/3 edits as 1/3 and saves back as exactly 1/3", () => {
    const stored = { num: 1, den: 3 };
    const text = rateToInput(stored);
    expect(text).toBe("1/3");
    expect(parseRate(text)).toEqual(stored);
  });

  it("round-trips every stored fraction exactly", () => {
    for (const f of [
      { num: 1, den: 3 },
      { num: 2, den: 7 },
      { num: 109, den: 10 },
      { num: 1093, den: 100000 },
      { num: 1, den: 1024 }, // 0.0009765625: 10 places, beyond the decimal limit
      { num: 9007199254740991, den: 9007199254740990 },
    ]) {
      expect(parseRate(rateToInput(f)), `${f.num}/${f.den}`).toEqual(f);
    }
  });
});

describe("exact rate display", () => {
  it("exactDecimal only answers for terminating fractions", () => {
    expect(exactDecimal({ num: 109, den: 10 })).toBe("10.9");
    expect(exactDecimal({ num: 1, den: 8 })).toBe("0.125");
    expect(exactDecimal({ num: 1, den: 3 })).toBeNull();
  });

  it("fractionToFixed rounds half-up with integer math", () => {
    expect(fractionToFixed({ num: 1, den: 3 }, 4)).toBe("0.3333");
    expect(fractionToFixed({ num: 2, den: 3 }, 4)).toBe("0.6667");
    expect(fractionToFixed({ num: 1, den: 8 }, 2)).toBe("0.13");
    expect(fractionToFixed({ num: 5, den: 1 }, 0)).toBe("5");
  });

  it("formatRate marks a rounded figure with ≈", () => {
    expect(formatRate({ num: 109, den: 10 })).toBe("10.9000");
    expect(formatRate({ num: 1, den: 3 })).toBe("≈0.3333");
    expect(formatRate({ num: 1, den: 3 }, 6)).toBe("≈0.333333");
  });
});

describe("describeRateChange", () => {
  it("shows the delta against the current rate", () => {
    expect(describeRateChange({ num: 109, den: 10 }, { num: 11, den: 1 })).toBe(
      "10.9000 → 11.0000 (+0.92%)",
    );
    expect(describeRateChange({ num: 109, den: 10 }, { num: 10, den: 1 })).toBe(
      "10.9000 → 10.0000 (-8.26%)",
    );
  });

  it("recognises an unchanged rate even when written differently", () => {
    expect(describeRateChange({ num: 109, den: 10 }, { num: 218, den: 20 })).toBe(
      "10.9000 (unchanged)",
    );
  });

  it("detects a change smaller than the displayed precision", () => {
    // 1/3 → 3333/10000 renders "≈0.3333 → 0.3333" but is NOT unchanged.
    expect(describeRateChange({ num: 1, den: 3 }, { num: 3333, den: 10000 })).toBe(
      "≈0.3333 → 0.3333 (-0.01%)",
    );
  });

  it("labels a first-time rate", () => {
    expect(describeRateChange(null, { num: 109, den: 10 })).toBe("new rate 10.9000");
  });
});
