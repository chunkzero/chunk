package example.arena;

import net.kyori.adventure.text.Component;
import net.kyori.adventure.text.format.NamedTextColor;
import net.minestom.server.color.Color;
import net.minestom.server.coordinate.Pos;

/** The two sides: red holds the west camp, blue the east. */
enum Team {
    RED("Red", NamedTextColor.RED, new Color(0xB02E26), new Pos(-40.5, 65, 0.5, -90, 0)),
    BLUE("Blue", NamedTextColor.BLUE, new Color(0x3C44AA), new Pos(40.5, 65, 0.5, 90, 0));

    final String title;
    final NamedTextColor color;
    final Color armour;
    final Pos spawn;

    Team(String title, NamedTextColor color, Color armour, Pos spawn) {
        this.title = title;
        this.color = color;
        this.armour = armour;
        this.spawn = spawn;
    }

    Team opponent() {
        return this == RED ? BLUE : RED;
    }

    Component label() {
        return Component.text(title, color);
    }
}
