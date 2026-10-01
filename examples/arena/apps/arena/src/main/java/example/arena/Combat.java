package example.arena;

import dev.chunkzero.runtime.SessionScope;

import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.minestom.server.component.DataComponents;
import net.minestom.server.entity.EquipmentSlot;
import net.minestom.server.entity.GameMode;
import net.minestom.server.entity.Player;
import net.minestom.server.entity.damage.Damage;
import net.minestom.server.event.entity.EntityAttackEvent;
import net.minestom.server.event.item.ItemDropEvent;
import net.minestom.server.event.player.PlayerMoveEvent;
import net.minestom.server.item.ItemStack;
import net.minestom.server.item.Material;

import java.time.Duration;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.function.BooleanSupplier;

/** Melee PvP: sword hits, deaths, a short respawn at the team spawn and the void below y = 40. */
final class Combat {
    private static final float SWORD_DAMAGE = 5;
    private static final float FIST_DAMAGE = 1;
    private static final long SWING_COOLDOWN_MILLIS = 500;
    private static final long VOID_CREDIT_MILLIS = 10_000;
    private static final Duration RESPAWN = Duration.ofSeconds(3);
    private static final double VOID_Y = 40;

    private final SessionScope scope;
    private final Map<Player, Fighter> fighters;
    private final Hud hud;
    private final BooleanSupplier fighting;

    Combat(SessionScope scope, Map<Player, Fighter> fighters, Hud hud, BooleanSupplier fighting) {
        this.scope = scope;
        this.fighters = fighters;
        this.hud = hud;
        this.fighting = fighting;
        scope.getEvents()
                .addListener(EntityAttackEvent.class, this::onAttack)
                .addListener(PlayerMoveEvent.class, this::onMove)
                .addListener(ItemDropEvent.class, event -> event.setCancelled(true));
    }

    /** Heals and equips a fighter at their team's spawn. */
    CompletableFuture<Void> spawn(Player player, Fighter fighter) {
        fighter.alive = true;
        player.setGameMode(GameMode.ADVENTURE);
        player.heal();
        player.setFood(20);
        var inventory = player.getInventory();
        inventory.clear();
        inventory.setItemStack(0, ItemStack.of(Material.IRON_SWORD));
        player.setEquipment(EquipmentSlot.HELMET, armour(Material.LEATHER_HELMET, fighter.team));
        player.setEquipment(
                EquipmentSlot.CHESTPLATE, armour(Material.LEATHER_CHESTPLATE, fighter.team));
        player.setEquipment(
                EquipmentSlot.LEGGINGS, armour(Material.LEATHER_LEGGINGS, fighter.team));
        player.setEquipment(EquipmentSlot.BOOTS, armour(Material.LEATHER_BOOTS, fighter.team));
        return player.teleport(fighter.team.spawn);
    }

    private void onAttack(EntityAttackEvent event) {
        if (!(event.getEntity() instanceof Player attacker)
                || !(event.getTarget() instanceof Player target)) return;
        var hitter = fighters.get(attacker);
        var victim = fighters.get(target);
        if (!fighting.getAsBoolean()
                || hitter == null
                || victim == null
                || !hitter.alive
                || !victim.alive
                || hitter.team == victim.team) return;
        var now = System.currentTimeMillis();
        if (now - hitter.lastSwing < SWING_COOLDOWN_MILLIS) return;
        hitter.lastSwing = now;
        victim.lastAttacker = attacker;
        victim.lastHit = now;
        var damage =
                attacker.getItemInMainHand().material() == Material.IRON_SWORD
                        ? SWORD_DAMAGE
                        : FIST_DAMAGE;
        if (target.getHealth() <= damage) {
            die(target, victim, attacker);
            return;
        }
        target.damage(Damage.fromPlayer(attacker, damage));
        var yaw = Math.toRadians(attacker.getPosition().yaw());
        target.takeKnockback(0.4f, Math.sin(yaw), -Math.cos(yaw));
    }

    private void onMove(PlayerMoveEvent event) {
        var player = event.getPlayer();
        var fighter = fighters.get(player);
        if (event.getNewPosition().y() >= VOID_Y || fighter == null || !fighter.alive) return;
        if (!fighting.getAsBoolean()) {
            event.setNewPosition(fighter.team.spawn);
            return;
        }
        var recentlyHit = System.currentTimeMillis() - fighter.lastHit < VOID_CREDIT_MILLIS;
        die(player, fighter, recentlyHit ? fighter.lastAttacker : null);
    }

    private void die(Player player, Fighter fighter, Player killer) {
        fighter.alive = false;
        fighter.deaths++;
        var credited = killer == null ? null : fighters.get(killer);
        Component cause;
        if (credited != null) {
            credited.kills++;
            hud.scored(killer);
            cause =
                    Component.text("Slain by ")
                            .append(Component.text(killer.getUsername(), credited.team.color));
        } else {
            cause = Component.text("Fell into the void");
        }
        var name = Component.text(player.getUsername(), fighter.team.color);
        var notice =
                name.append(Component.text(": ", NamedTextColor.GRAY))
                        .append(cause.color(NamedTextColor.GRAY));
        fighters.keySet().forEach(viewer -> viewer.sendMessage(notice));
        player.setGameMode(GameMode.SPECTATOR);
        hud.died(player, cause);
        scope.getScheduler()
                .buildTask(
                        () -> {
                            if (fighters.get(player) == fighter) spawn(player, fighter);
                        })
                .delay(RESPAWN)
                .schedule();
    }

    private static ItemStack armour(Material material, Team team) {
        return ItemStack.of(material).with(DataComponents.DYED_COLOR, team.armour);
    }
}
