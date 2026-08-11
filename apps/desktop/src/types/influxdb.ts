export type InfluxDbVersion = "1" | "2" | "3";

export interface InfluxDbExternalConfig {
  version?: InfluxDbVersion;
  org?: string;
}

/** InfluxDB 1.x/2.x listen on 8086; InfluxDB 3.x defaults to 8181. */
export const INFLUXDB_DEFAULT_PORTS: Record<InfluxDbVersion, number> = {
  "1": 8086,
  "2": 8086,
  "3": 8181,
};

export function normalizeInfluxDbVersion(value: unknown): InfluxDbVersion {
  const normalized = typeof value === "string" ? value.trim().replace(/^v/i, "") : "";
  return normalized === "2" || normalized === "3" ? normalized : "1";
}

/** 2.x scopes data by bucket, 3.x went back to calling it a database. */
export function influxDbUsesToken(version: InfluxDbVersion): boolean {
  return version === "2" || version === "3";
}
