//! Creating, joining and leaving a group, and losing it.

use std::mem;

use jellyfin::GroupRequest;

use crate::receiver::group::Group;
use crate::receiver::{Receiver, ReceiverAction};

impl Receiver {
    /// The joined group's id.
    #[must_use]
    pub fn group_id(&self) -> Option<&str> {
        self.group.as_ref().map(|g| g.id.as_str())
    }

    pub(in crate::receiver) fn group_create(
        &mut self,
        name: String,
        actions: &mut Vec<ReceiverAction>,
    ) {
        self.creating_group = true;
        actions.push(ReceiverAction::Group(GroupRequest::New { name }));
    }

    pub(in crate::receiver) fn group_join(
        &mut self,
        group_id: String,
        actions: &mut Vec<ReceiverAction>,
    ) {
        self.creating_group = false;
        actions.push(ReceiverAction::Group(GroupRequest::Join { group_id }));
    }

    /// Out at once: a Leave that fails during an outage must not leave a ghost
    /// group, and commands racing the `GroupLeft` must not be applied.
    pub(in crate::receiver) fn group_leave(&mut self, actions: &mut Vec<ReceiverAction>) {
        actions.push(ReceiverAction::Group(GroupRequest::Leave));
        self.drop_group(actions);
    }

    pub(in crate::receiver) fn group_joined(
        &mut self,
        group_id: String,
        actions: &mut Vec<ReceiverAction>,
    ) {
        let rejoined = self.group.replace(Group::new(group_id)).is_some();
        if rejoined {
            self.stop_following(actions);
        }
        // A rejoin is not the group a failed create asked for; its queue stays.
        if mem::take(&mut self.creating_group)
            && !rejoined
            && let Some(position) = self.playing_position()
        {
            actions.push(ReceiverAction::GroupPublishQueue { position });
        }
    }

    pub(in crate::receiver) fn drop_group(&mut self, actions: &mut Vec<ReceiverAction>) {
        if self.group.take().is_some() {
            self.stop_following(actions);
        }
    }

    /// The message is shown even with no group: a refused join ends up here.
    pub(in crate::receiver) fn group_gone(
        &mut self,
        message: String,
        actions: &mut Vec<ReceiverAction>,
    ) {
        self.creating_group = false;
        self.drop_group(actions);
        actions.push(ReceiverAction::ShowMessage { text: message });
    }
}
