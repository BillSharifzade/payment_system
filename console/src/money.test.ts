import { describe, expect, it } from "vitest";
import { decimalToFraction, describeRateChange, formatMinor, toMinor } from "./money";

describe("toMinor", () => {
  it("parses operator-typed amounts exactly", () => {
    expect(toMinor("1500.50")).toBe(150050);
    expect(toMinor("1 500,50")).toBe(150050);
    expect(toMinor("5")).toBe(500);
    expect(toMinor("5.")).toBe(500);
    expect(toMinor("5.1")).toBe(510);
    expect(toMinor("00.29")).toBe(29); // a float multiply would give 28.999…
  });

  it("rejects zero, negatives, too many decimals and junk", () => {
    for (const bad of ["", "0", "0.00", "-5", "1.234", "abc", "1e3", "5,5,5"]) {
      expect(toMinor(bad), bad).toBeNull();
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

  it("rejects non-positive, over-precise and malformed input", () => {
    for (const bad of ["", "0", "0.0", "-1", "1.123456789", "abc", "1,5", "1e3"]) {
      expect(decimalToFraction(bad), bad).toBeNull();
    }
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

  it("labels a first-time rate", () => {
    expect(describeRateChange(null, { num: 109, den: 10 })).toBe("new rate 10.9000");
  });
});

describe("formatMinor", () => {
  it("renders minor units as a two-decimal amount", () => {
    expect(formatMinor(5, "TJS")).toBe("0.05 TJS");
    expect(formatMinor(0, "TJS")).toBe("0.00 TJS");
    expect(formatMinor(-150, "USD")).toBe("-1.50 USD");
    // Thousands grouping follows the runtime locale; compare against it.
    expect(formatMinor(123456, "TJS")).toBe(`${(1234).toLocaleString()}.56 TJS`);
  });
});
