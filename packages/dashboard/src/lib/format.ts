import type { Timestamp } from "@bufbuild/protobuf/wkt";
import { timestampDate } from "@bufbuild/protobuf/wkt";

import { DeploymentState, LogSeverity, LogSource } from "../gen/chunk/management/v1/common_pb.ts";
import { DeploymentTrigger } from "../gen/chunk/management/v1/deployments_pb.ts";
import { DomainState } from "../gen/chunk/management/v1/domains_pb.ts";
import { EnvironmentState } from "../gen/chunk/management/v1/projects_pb.ts";

export type Tone = "success" | "warning" | "danger" | "muted";
export interface Status {
  label: string;
  tone: Tone;
}

export const environmentStatus: Record<EnvironmentState, Status> = {
  [EnvironmentState.UNSPECIFIED]: { label: "Unknown", tone: "muted" },
  [EnvironmentState.PENDING]: { label: "Not deployed", tone: "muted" },
  [EnvironmentState.STARTING]: { label: "Starting", tone: "warning" },
  [EnvironmentState.RUNNING]: { label: "Running", tone: "success" },
  [EnvironmentState.SUSPENDED]: { label: "Suspended", tone: "muted" },
  [EnvironmentState.DELETING]: { label: "Deleting", tone: "danger" },
};

export const deploymentStatus: Record<DeploymentState, Status> = {
  [DeploymentState.UNSPECIFIED]: { label: "Unknown", tone: "muted" },
  [DeploymentState.PENDING]: { label: "Pending", tone: "warning" },
  [DeploymentState.IN_PROGRESS]: { label: "In progress", tone: "warning" },
  [DeploymentState.ACTIVE]: { label: "Active", tone: "success" },
  [DeploymentState.FAILED]: { label: "Failed", tone: "danger" },
  [DeploymentState.SUPERSEDED]: { label: "Superseded", tone: "muted" },
};

export const domainStatus: Record<DomainState, Status> = {
  [DomainState.UNSPECIFIED]: { label: "Unknown", tone: "muted" },
  [DomainState.PENDING_VERIFICATION]: { label: "Pending verification", tone: "warning" },
  [DomainState.VERIFIED]: { label: "Verified", tone: "success" },
};

export const triggerLabels: Record<DeploymentTrigger, string> = {
  [DeploymentTrigger.UNSPECIFIED]: "",
  [DeploymentTrigger.DEPLOY]: "Deploy",
  [DeploymentTrigger.PROMOTE]: "Promote",
  [DeploymentTrigger.ROLLBACK]: "Rollback",
};

export const severityLabels: Record<LogSeverity, string> = {
  [LogSeverity.UNSPECIFIED]: "",
  [LogSeverity.DEBUG]: "DEBUG",
  [LogSeverity.INFO]: "INFO",
  [LogSeverity.WARN]: "WARN",
  [LogSeverity.ERROR]: "ERROR",
};

export const sourceLabels: Record<LogSource, string> = {
  [LogSource.UNSPECIFIED]: "",
  [LogSource.CORE]: "core",
  [LogSource.GATEWAY]: "gateway",
  [LogSource.EXEC]: "exec",
  [LogSource.JVM]: "jvm",
};

const units: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 31_536_000],
  ["month", 2_592_000],
  ["day", 86_400],
  ["hour", 3_600],
  ["minute", 60],
];
const relative = new Intl.RelativeTimeFormat("en", { numeric: "auto" });

export function timeAgo(timestamp: Timestamp | undefined) {
  if (!timestamp) return "";
  const seconds = Math.round((timestampDate(timestamp).getTime() - Date.now()) / 1000);
  for (const [unit, size] of units) {
    if (Math.abs(seconds) >= size) return relative.format(Math.round(seconds / size), unit);
  }
  return "just now";
}

export const clock = new Intl.DateTimeFormat("en", { hour12: false, timeStyle: "medium" });
