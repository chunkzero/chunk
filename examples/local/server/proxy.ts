import { defineFunctions, v } from '@chunk/server';
import schema from './schema/index.ts';

const { query } = defineFunctions(schema);
const user = { uuid: v.string(), username: v.string() };
const destination = v.object({ key: v.string(), session_type: v.string(), machine_profile: v.string() });

export const status = query({ args: { host: v.string() }, returns: v.object({ motd: v.string(), online: v.integer(), max: v.integer() }), handler: ({ db }) => ({
  motd: db.query('settings').withIndex('by_name', q => q.eq('name', 'server')).unique()?.motd ?? 'chunk typed backend | Lobby + Arena',
  online: 0, max: 32,
}) });
export const admit = query({ args: user, returns: v.object({ allow: v.boolean(), reason: v.string() }), handler: ({ db }) => ({
  allow: db.query('settings').withIndex('by_name', q => q.eq('name', 'server')).unique()?.admission !== 'deny',
  reason: 'The local example is closed for maintenance.',
}) });
export const route = query({ args: user, returns: destination, handler: () => ({ key: 'lobby', session_type: 'lobby', machine_profile: 'local' }) });
export const move = query({ args: { ...user, destination }, returns: destination, handler: (_, { destination }) => {
  if (!['lobby', 'arena'].includes(destination.session_type)) throw new Error('Unknown destination');
  return destination;
} });
