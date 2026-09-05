import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { getStatus } from "../lib/api";

export function Overview() {
    const [token, setToken] = useState("");
    const [credential, setCredential] = useState("");
    const [connection, setConnection] = useState(0);
    const client = useQueryClient();
    const status = useQuery({
        queryKey: ["status", connection],
        queryFn: ({ signal }) => getStatus(credential, signal),
        enabled: credential.length > 0,
        retry: false,
        refetchInterval: 10_000,
    });

    return (
        <>
            <section className="intro">
                <p className="eyebrow">APPLICATION OVERVIEW</p>
                <h1>A home for your server.</h1>
                <p>Connect to your backend to inspect the services available in this build.</p>
            </section>
            <section className="panel">
                <div className="panel-heading">
                    <h2>Backend connection</h2>
                    <span className="badge">
                        {status.isError
                            ? "Connection failed"
                            : status.isSuccess
                              ? "Connected"
                              : "Not connected"}
                    </span>
                </div>
                {!credential || status.isError ? (
                    <form
                        onSubmit={(event) => {
                            event.preventDefault();
                            client.clear();
                            setConnection((value) => value + 1);
                            setCredential(token.trim());
                            setToken("");
                        }}
                    >
                        <label htmlFor="token">Management token</label>
                        <div className="form-row">
                            <input
                                id="token"
                                type="password"
                                required
                                autoComplete="off"
                                value={token}
                                onChange={(event) => setToken(event.target.value)}
                                placeholder="Enter your backend token"
                            />
                            <button type="submit" disabled={!token.trim()}>
                                Connect
                            </button>
                        </div>
                        <p className="hint">
                            Kept in memory for this page only. Refreshing signs you out.
                        </p>
                    </form>
                ) : (
                    <div className="connection-row">
                        <p>{status.isPending ? "Connecting…" : `chunk ${status.data?.version}`}</p>
                        <button
                            className="secondary"
                            onClick={() => {
                                setCredential("");
                                client.clear();
                            }}
                        >
                            Disconnect
                        </button>
                    </div>
                )}
                {status.error && (
                    <p role="alert" className="error">
                        {status.error.message}
                    </p>
                )}
            </section>
            {status.data && (
                <section className="services" aria-label="Backend capabilities">
                    {[
                        {
                            title: "Functions",
                            available: status.data.functions,
                            description: "Application functions and reactive data.",
                        },
                        {
                            title: "Gameplay",
                            available: status.data.reconciliation,
                            description: "Session placement and runtime lifecycle.",
                        },
                        {
                            title: "Assets",
                            available: status.data.asset_uploads,
                            description: "Immutable files and maps pinned to deployments.",
                        },
                    ].map((service) => (
                        <article className="panel" key={service.title}>
                            <h2>{service.title}</h2>
                            <p>{service.description}</p>
                            <span className="badge">
                                {service.available ? "Available" : "Not implemented in this build"}
                            </span>
                        </article>
                    ))}
                </section>
            )}
        </>
    );
}
