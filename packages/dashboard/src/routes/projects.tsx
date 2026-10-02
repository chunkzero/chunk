import { Add01Icon } from "@hugeicons/core-free-icons";
import * as stylex from "@stylexjs/stylex";
import { useState } from "react";

import { ConfirmDialog } from "../components/confirm-dialog.tsx";
import { DeployGuideLink } from "../components/deploy-guide.tsx";
import { ListLink } from "../components/list-link.tsx";
import { EmptyState, ErrorText, Page } from "../components/page.tsx";
import { listStyles } from "../components/panel.tsx";
import { Button } from "../components/ui/button.tsx";
import { Input } from "../components/ui/input.tsx";
import { Label } from "../components/ui/label.tsx";
import { Select } from "../components/ui/select.tsx";
import type { Owner } from "../gen/chunk/management/v1/auth_pb.ts";
import { api, errorMessage, refresh, useRequestId } from "../lib/client.ts";
import { timeAgo } from "../lib/format.ts";
import { usePrincipal, useProjects } from "../lib/queries.ts";
import { colors, fontSizes, lineHeights, radii, space } from "../tokens.stylex.ts";

const styles = stylex.create({
  heading: { display: "flex", alignItems: "center", justifyContent: "space-between", gap: space.s4 },
  title: { fontSize: fontSizes.xl, lineHeight: lineHeights.xl, fontWeight: 600, letterSpacing: "-0.025em" },
  list: {
    overflow: "hidden",
    borderWidth: "1px",
    borderStyle: "solid",
    borderColor: colors.border,
    borderRadius: radii.lg,
    backgroundColor: colors.card,
  },
  name: { flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", fontWeight: 500 },
  detail: { fontSize: fontSizes.xs, lineHeight: lineHeights.xs, color: colors.mutedForeground },
  field: { display: "flex", flexDirection: "column", gap: space.s2 },
});

export function Projects() {
  const projects = useProjects();
  // Owners matter only to callers who can choose between several.
  const principal = usePrincipal();
  const owners = principal.data?.owners ?? [];
  const ownerNames = owners.length > 1 ? new Map(owners.map((owner) => [owner.id, owner.displayName])) : undefined;
  const [creating, setCreating] = useState(false);

  return (
    <Page>
      <div {...stylex.props(styles.heading)}>
        <h1 {...stylex.props(styles.title)}>Projects</h1>
        <Button
          size="sm"
          variant="outline"
          icon={Add01Icon}
          onClick={async () => {
            // Memberships may have changed since the principal was loaded; the dialog starts from the current ones.
            await principal.refetch();
            setCreating(true);
          }}
        >
          New project
        </Button>
      </div>
      {projects.data?.length === 0 ? (
        <EmptyState>
          <p>
            No projects yet. Create one, then deploy to it as the <DeployGuideLink /> shows.
          </p>
        </EmptyState>
      ) : (
        <ul {...stylex.props(styles.list)}>
          {projects.data?.map((project) => (
            <li key={project.id} {...stylex.props(listStyles.row)}>
              <ListLink to="/p/$project" params={{ project: project.id }}>
                <span {...stylex.props(styles.name)}>{project.name}</span>
                {ownerNames && (
                  <span {...stylex.props(styles.detail)}>{ownerNames.get(project.ownerId) ?? project.ownerId}</span>
                )}
                <span {...stylex.props(styles.detail)}>Created {timeAgo(project.createTime)}</span>
              </ListLink>
            </li>
          ))}
        </ul>
      )}
      <ErrorText error={projects.error ? errorMessage(projects.error) : undefined} />
      {creating && <CreateProjectDialog owners={owners} onClose={() => setCreating(false)} />}
    </Page>
  );
}

/** Creates a project; callers with several owners pick its owner, and the service picks the only one otherwise. */
function CreateProjectDialog({ owners, onClose }: { owners: readonly Owner[]; onClose: () => void }) {
  const [name, setName] = useState("");
  const [ownerId, setOwnerId] = useState(owners.length > 1 ? (owners[0]?.id ?? "") : "");
  const requestId = useRequestId(name, ownerId);
  return (
    <ConfirmDialog
      title="New project"
      description="A project holds a game's environments, such as staging and production."
      confirmLabel="Create"
      disabled={!name.trim()}
      action={async () => {
        await api.projects.createProject({ requestId, name: name.trim(), ownerId });
        await refresh("projects");
      }}
      onClose={onClose}
    >
      <div {...stylex.props(styles.field)}>
        <Label htmlFor="project-name">Name</Label>
        <Input
          id="project-name"
          required
          autoFocus
          autoComplete="off"
          placeholder="survival"
          value={name}
          onValueChange={setName}
        />
      </div>
      {owners.length > 1 && (
        <div {...stylex.props(styles.field)}>
          <Label htmlFor="project-owner">Owner</Label>
          <Select
            id="project-owner"
            value={ownerId}
            onValueChange={setOwnerId}
            items={owners.map((owner) => ({ value: owner.id, label: owner.displayName }))}
          />
        </div>
      )}
    </ConfirmDialog>
  );
}
