use chunk_proto::v1::{PlayerStatus, SessionDemand};

use super::{Model, Tab};
use crate::local::{Command, report::Deployment};

pub(in super::super) enum Input {
    Search,
    Move(MoveForm),
}

pub(in super::super) struct MoveForm {
    pub deployment: String,
    pub player: String,
    pub name: String,
    /// The release's session types and their machine profiles, captured when the form opens.
    pub session_types: Vec<(String, String)>,
    pub choice: usize,
    pub key: String,
}

impl Model {
    /// Players matching the search, by name, with the release that serves each.
    pub fn roster(&self) -> Vec<(&Deployment, &PlayerStatus)> {
        let filter = self.filter.to_lowercase();
        let mut players: Vec<_> = self
            .deployments
            .iter()
            .flat_map(|deployment| deployment.players.iter().map(move |player| (deployment, player)))
            .filter(|(_, player)| filter.is_empty() || matches(player, &filter))
            .collect();
        players.sort_by_key(|(_, player)| name(player).to_lowercase());
        players
    }

    pub fn player_count(&self) -> usize {
        self.deployments.iter().map(|deployment| deployment.players.len()).sum()
    }

    pub fn selected_player(&self) -> Option<(usize, &Deployment, &PlayerStatus)> {
        let roster = self.roster();
        let index = self.player.as_deref().and_then(|id| roster.iter().position(|(_, p)| id_of(p) == id));
        let index = index.unwrap_or(0);
        roster.get(index).map(|&(deployment, player)| (index, deployment, player))
    }

    pub fn move_player(&mut self, offset: isize) {
        let index = self.selected_player().map_or(0, |(index, ..)| index);
        let roster = self.roster();
        let index = index.saturating_add_signed(offset).min(roster.len().saturating_sub(1));
        self.player = roster.get(index).map(|(_, player)| id_of(player).to_owned());
    }

    pub fn search(&mut self) {
        if self.tab() == Tab::Players {
            self.input = Some(Input::Search);
        }
    }

    pub fn clear_search(&mut self) {
        self.filter.clear();
    }

    /// Opens the move form for the selected player with their current session type chosen.
    pub fn start_move(&mut self) {
        let Some((_, deployment, player)) = self.selected_player() else { return };
        let session_types: Vec<_> = deployment.session_types.clone().into_iter().collect();
        let current = player.demand.as_ref().map(|demand| demand.session_type.as_str());
        let choice = session_types.iter().position(|(name, _)| Some(name.as_str()) == current).unwrap_or(0);
        self.input = Some(Input::Move(MoveForm {
            deployment: deployment.id.clone(),
            player: id_of(player).to_owned(),
            name: name(player).to_owned(),
            session_types,
            choice,
            key: String::new(),
        }));
    }

    pub fn type_char(&mut self, character: char) {
        if let Some(text) = self.editing() {
            text.push(character);
        }
        self.follow_search();
    }

    pub fn backspace(&mut self) {
        if let Some(text) = self.editing() {
            text.pop();
        }
        self.follow_search();
    }

    /// Up/down moves the player selection while searching and the session type in the move form.
    pub fn arrow(&mut self, offset: isize) {
        match &mut self.input {
            Some(Input::Move(form)) => {
                form.choice = form.choice.saturating_add_signed(offset).min(form.session_types.len().saturating_sub(1));
            }
            _ => self.move_player(offset),
        }
    }

    /// Keeps a search's filter, or turns a complete move form into a command.
    pub fn submit(&mut self) -> Option<Command> {
        match self.input.take()? {
            Input::Search => None,
            Input::Move(mut form) => {
                if form.key.is_empty() || form.choice >= form.session_types.len() {
                    self.input = Some(Input::Move(form));
                    return None;
                }
                let (session_type, machine_profile) = form.session_types.swap_remove(form.choice);
                Some(Command::MovePlayer {
                    deployment: form.deployment,
                    player: form.player,
                    name: form.name,
                    demand: SessionDemand { session_type, key: form.key, machine_profile },
                })
            }
        }
    }

    /// Escape abandons the move form, or clears the search being typed.
    pub fn cancel(&mut self) {
        if matches!(self.input.take(), Some(Input::Search)) {
            self.filter.clear();
        }
    }

    fn editing(&mut self) -> Option<&mut String> {
        match self.input.as_mut()? {
            Input::Search => Some(&mut self.filter),
            Input::Move(form) => Some(&mut form.key),
        }
    }

    /// Selects the first match whenever the current selection is filtered out.
    fn follow_search(&mut self) {
        if matches!(self.input, Some(Input::Search)) {
            self.move_player(0);
        }
    }
}

fn matches(player: &PlayerStatus, filter: &str) -> bool {
    let demand = player.demand.as_ref();
    [name(player), id_of(player), &player.app_id, &player.host_id]
        .into_iter()
        .chain(demand.map(|d| d.session_type.as_str()))
        .chain(demand.map(|d| d.key.as_str()))
        .any(|field| field.to_lowercase().contains(filter))
}

pub(in super::super) fn name(player: &PlayerStatus) -> &str {
    player.identity.as_ref().map_or("", |identity| identity.username.as_str())
}

pub(in super::super) fn id_of(player: &PlayerStatus) -> &str {
    player.identity.as_ref().map_or("", |identity| identity.uuid.as_str())
}
