import { describe, expect, it } from "vitest";

import {
  INFLUXDB_DEFAULT_PORTS,
  influxDbUsesToken,
  normalizeInfluxDbVersion,
} from "@/types/influxdb";

describe("normalizeInfluxDbVersion", () => {
  it("accepts bare and v-prefixed versions", () => {
    expect(normalizeInfluxDbVersion("1")).toBe("1");
    expect(normalizeInfluxDbVersion("2")).toBe("2");
    expect(normalizeInfluxDbVersion("3")).toBe("3");
    expect(normalizeInfluxDbVersion("v3")).toBe("3");
    expect(normalizeInfluxDbVersion("V2")).toBe("2");
    expect(normalizeInfluxDbVersion(" 3 ")).toBe("3");
  });

  it("falls back to 1.x for unknown or missing values", () => {
    expect(normalizeInfluxDbVersion(undefined)).toBe("1");
    expect(normalizeInfluxDbVersion(null)).toBe("1");
    expect(normalizeInfluxDbVersion("4")).toBe("1");
    expect(normalizeInfluxDbVersion(3)).toBe("1");
  });
});

describe("influxDbUsesToken", () => {
  it("marks 2.x and 3.x as token based", () => {
    expect(influxDbUsesToken("1")).toBe(false);
    expect(influxDbUsesToken("2")).toBe(true);
    expect(influxDbUsesToken("3")).toBe(true);
  });
});

describe("INFLUXDB_DEFAULT_PORTS", () => {
  it("uses 8086 for 1.x/2.x and 8181 for 3.x", () => {
    expect(INFLUXDB_DEFAULT_PORTS["1"]).toBe(8086);
    expect(INFLUXDB_DEFAULT_PORTS["2"]).toBe(8086);
    expect(INFLUXDB_DEFAULT_PORTS["3"]).toBe(8181);
  });
});
