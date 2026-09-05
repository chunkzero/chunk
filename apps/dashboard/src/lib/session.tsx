import { useQueryClient } from "@tanstack/react-query";
import { createContext, useContext, useState, type ReactNode } from "react";

interface Session {
    token: string | null;
    connect: (token: string) => void;
    disconnect: () => void;
}

const SessionContext = createContext<Session | null>(null);

/// The management token lives in memory only, so a refresh signs the operator out.
export function SessionProvider({ children }: { children: ReactNode }) {
    const client = useQueryClient();
    const [token, setToken] = useState<string | null>(null);
    return (
        <SessionContext.Provider
            value={{
                token,
                connect: setToken,
                disconnect: () => {
                    setToken(null);
                    client.clear();
                },
            }}
        >
            {children}
        </SessionContext.Provider>
    );
}

export function useSession() {
    const session = useContext(SessionContext);
    if (!session) throw new Error("useSession requires SessionProvider");
    return session;
}
