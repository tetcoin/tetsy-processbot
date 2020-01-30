use crate::db::*;
use crate::local_state::*;
use crate::{
	bots, constants::*, duration_ticks::DurationTicks, error, github, matrix,
	process, Result,
};
use snafu::OptionExt;
use std::time::{Duration, SystemTime};

impl bots::Bot {
	/// Return the project card attached to an issue, if there is one, and the user who attached it
	pub async fn issue_actor_and_project_card(
		&self,
		repo_name: &str,
		issue_number: i64,
	) -> Result<Option<(github::User, github::ProjectCard)>> {
		Ok(self
			.github_bot
			.active_project_event(repo_name, issue_number)
			.await?
			.and_then(|mut issue_event| {
				issue_event
					.project_card
					.take()
					.map(|card| (issue_event.actor, card))
			}))
	}

	async fn author_special_attach_only_project(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		(project, process_info): (&github::Project, &process::ProcessInfo),
	) -> Result<()> {
		// get the project's backlog column or use the organization-wide default
		if let Some(backlog_column) = self
			.github_bot
			.project_column_by_name(
				project,
				process_info
					.backlog
					.as_ref()
					.unwrap_or(&self.config.project_backlog_column_name),
			)
			.await?
		{
			local_state.update_issue_project(
				Some(IssueProject {
					state: IssueProjectState::Confirmed,
					actor_login: issue.user.login.clone(),
					project_column_id: backlog_column.id,
				}),
				&self.db,
			)?;
			self.github_bot
				.create_project_card(
					backlog_column.id,
					issue.id.context(error::MissingData)?,
					github::ProjectCardContentType::Issue,
				)
				.await?;
		} else {
			log::warn!(
				"{project_url} needs a backlog column",
				project_url =
					project.html_url.as_ref().context(error::MissingData)?,
			);
			self.matrix_bot.send_to_room(
				&process_info.matrix_room_id,
				&PROJECT_NEEDS_BACKLOG
					.replace("{owner}", process_info.owner_or_delegate())
					.replace(
						"{project_url}",
						project
							.html_url
							.as_ref()
							.context(error::MissingData)?,
					),
			)?;
		}
		Ok(())
	}

	async fn issue_author_core_no_project(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		since: Option<Duration>,
	) -> Result<()> {
		let issue_html_url =
			issue.html_url.as_ref().context(error::MissingData)?;
		let ticks = since.ticks(self.config.no_project_author_is_core_ping);
		match ticks {
			None => {
				local_state.update_issue_no_project_ping(
					Some(SystemTime::now()),
					&self.db,
				)?;
				self.matrix_bot.send_to_default(
					&WILL_CLOSE_FOR_NO_PROJECT
						.replace("{author}", &issue.user.login)
						.replace("{issue_url}", issue_html_url),
				)?;
			}
			Some(0) => {}
			Some(i) => {
				if i >= self.config.no_project_author_is_core_close_pr
					/ self.config.no_project_author_is_core_ping
				{
					// If after some timeout there is still no project
					// attached, move the issue to Core Sorting
					// repository
					self.github_bot
						.close_issue(
							&issue
								.repository
								.as_ref()
								.context(error::MissingData)?
								.name,
							issue.number,
						)
						.await?;
					self.github_bot
						.create_issue(
							&self.config.core_sorting_repo_name,
							issue.title.as_ref().unwrap_or(&"".to_owned()),
							issue.body.as_ref().unwrap_or(&"".to_owned()),
							issue
								.assignee
								.as_ref()
								.map(|a| a.login.as_ref())
								.unwrap_or(&"".to_owned()),
						)
						.await?;
					local_state.delete(&self.db, &local_state.key)?;
				} else {
					local_state.update_issue_no_project_npings(i, &self.db)?;
					self.matrix_bot.send_to_default(
						&WILL_CLOSE_FOR_NO_PROJECT
							.replace("{author}", &issue.user.login)
							.replace("{issue_url}", issue_html_url),
					)?;
				}
			}
		}
		Ok(())
	}

	async fn issue_author_unknown_no_project(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		since: Option<Duration>,
	) -> Result<()> {
		let issue_html_url =
			issue.html_url.as_ref().context(error::MissingData)?;

		let ticks =
			since.ticks(self.config.no_project_author_not_core_close_pr);

		match ticks {
			None => {
				// send a message to the "Core Developers" room
				// on Riot with the title of the issue and a link.
				local_state.update_issue_no_project_ping(
					Some(SystemTime::now()),
					&self.db,
				)?;
				self.matrix_bot.send_to_default(
					&WILL_CLOSE_FOR_NO_PROJECT
						.replace("{author}", &issue.user.login)
						.replace("{issue_url}", issue_html_url),
				)?;
			}
			Some(0) => {}
			_ => {
				// If after some timeout there is still no project
				// attached, move the issue to Core Sorting
				// repository.
				self.github_bot
					.close_issue(
						&issue
							.repository
							.as_ref()
							.context(error::MissingData)?
							.name,
						issue.number,
					)
					.await?;
				self.github_bot
					.create_issue(
						&self.config.core_sorting_repo_name,
						issue.title.as_ref().unwrap_or(&"".to_owned()),
						issue.body.as_ref().unwrap_or(&"".to_owned()),
						&issue
							.assignee
							.as_ref()
							.map(|a| a.login.as_ref())
							.unwrap_or(String::new().as_ref()),
					)
					.await?;
				local_state.delete(&self.db, &local_state.key)?;
			}
		}
		Ok(())
	}

	fn author_non_special_project_state_none(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		project: &github::Project,
		project_column: &github::ProjectColumn,
		process_info: &process::ProcessInfo,
		actor: &github::User,
	) -> Result<()> {
		local_state.update_issue_confirm_project_ping(
			Some(SystemTime::now()),
			&self.db,
		)?;
		local_state.update_issue_project(
			Some(IssueProject {
				state: IssueProjectState::Unconfirmed,
				actor_login: actor.login.clone(),
				project_column_id: project_column.id,
			}),
			&self.db,
		)?;
		self.matrix_bot.send_to_room(
			&process_info.matrix_room_id,
			&PROJECT_CONFIRMATION
				.replace(
					"{issue_url}",
					issue.html_url.as_ref().context(error::MissingData)?,
				)
				.replace(
					"{project_url}",
					project.html_url.as_ref().context(error::MissingData)?,
				)
				.replace(
					"{issue_id}",
					&format!("{}", issue.id.context(error::MissingData)?),
				)
				.replace("{column_id}", &format!("{}", project_column.id))
				.replace(
					"{seconds}",
					&format!("{}", self.config.project_confirmation_timeout),
				),
		)?;
		Ok(())
	}

	async fn author_non_special_project_state_unconfirmed(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		project: &github::Project,
		project_column: &github::ProjectColumn,
		process_info: &process::ProcessInfo,
		actor: &github::User,
	) -> Result<()> {
		let issue_id = issue.id.context(error::MissingData)?;
		let issue_html_url =
			issue.html_url.as_ref().context(error::MissingData)?;

		let issue_project =
			local_state.issue_project().expect("has to be Some here");
		let unconfirmed_id = issue_project.project_column_id;

		if project_column.id != unconfirmed_id {
			local_state.update_issue_confirm_project_ping(
				Some(SystemTime::now()),
				&self.db,
			)?;
			local_state.update_issue_project(
				Some(IssueProject {
					state: IssueProjectState::Unconfirmed,
					actor_login: actor.login.clone(),
					project_column_id: project_column.id,
				}),
				&self.db,
			)?;
			self.matrix_bot.send_to_room(
				&process_info.matrix_room_id,
				&PROJECT_CONFIRMATION
					.replace(
						"{issue_url}",
						issue.html_url.as_ref().context(error::MissingData)?,
					)
					.replace(
						"{project_url}",
						project
							.html_url
							.as_ref()
							.context(error::MissingData)?,
					)
					.replace(
						"{issue_id}",
						&format!("{}", issue.id.context(error::MissingData)?),
					)
					.replace("{column_id}", &format!("{}", project_column.id))
					.replace(
						"{seconds}",
						&format!(
							"{}",
							self.config.project_confirmation_timeout
						),
					),
			)?;
		} else {
			let ticks = local_state
				.issue_confirm_project_ping()
				.and_then(|t| t.elapsed().ok())
				.ticks(self.config.project_confirmation_timeout);

			match ticks.expect("don't know how long to wait for confirmation; shouldn't ever allow issue_project_state to be set without updating issue_confirm_project_ping") {
			0 => {}
			_ => {
				// confirmation timeout. delete project card and reattach last
				// confirmed if possible
				local_state.update_issue_confirm_project_ping(None, &self.db)?;
				local_state.update_issue_project(
					local_state.last_confirmed_issue_project().cloned(),
					&self.db,
				)?;
				self.github_bot.delete_project_card(unconfirmed_id).await?;
				if let Some(prev_column_id) =
					local_state.issue_project().map(|p| p.project_column_id)
				{
					// reattach the last confirmed project
					self.github_bot.create_project_card(
						prev_column_id,
						issue_id,
						github::ProjectCardContentType::Issue,
					).await?;
				}
				if let Some(matrix_id) = self.github_to_matrix
					.get(&actor.login)
					.and_then(|matrix_id| matrix::parse_id(matrix_id))
				{
					self.matrix_bot.send_private_message(
                        &self.db,
						&matrix_id,
						&ISSUE_REVERT_PROJECT_NOTIFICATION
							.replace("{1}", &issue_html_url),
					)?;
				} else {
					// no matrix id to message
				}
			}
		}
		}
		Ok(())
	}

	async fn author_non_special_project_state_denied(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		project: &github::Project,
		project_column: &github::ProjectColumn,
		process_info: &process::ProcessInfo,
		actor: &github::User,
	) -> Result<()> {
		let issue_id = issue.id.context(error::MissingData)?;
		let issue_html_url =
			issue.html_url.as_ref().context(error::MissingData)?;
		let denied_id = local_state.issue_project().unwrap().project_column_id;

		if project_column.id != denied_id {
			local_state.update_issue_confirm_project_ping(
				Some(SystemTime::now()),
				&self.db,
			)?;
			local_state.update_issue_project(
				Some(IssueProject {
					state: IssueProjectState::Unconfirmed,
					actor_login: actor.login.clone(),
					project_column_id: project_column.id,
				}),
				&self.db,
			)?;
			self.matrix_bot.send_to_room(
				&process_info.matrix_room_id,
				&PROJECT_CONFIRMATION
					.replace(
						"{issue_url}",
						issue.html_url.as_ref().context(error::MissingData)?,
					)
					.replace(
						"{project_url}",
						project
							.html_url
							.as_ref()
							.context(error::MissingData)?,
					)
					.replace(
						"{issue_id}",
						&format!("{}", issue.id.context(error::MissingData)?),
					)
					.replace("{column_id}", &format!("{}", project_column.id))
					.replace(
						"{seconds}",
						&format!(
							"{}",
							self.config.project_confirmation_timeout
						),
					),
			)?;
		} else {
			local_state.update_issue_confirm_project_ping(None, &self.db)?;
			local_state.update_issue_project(
				local_state.last_confirmed_issue_project().cloned(),
				&self.db,
			)?;
			if let Some(prev_column_id) =
				local_state.issue_project().map(|p| p.project_column_id)
			{
				// reattach the last confirmed project
				self.github_bot
					.create_project_card(
						prev_column_id,
						issue_id,
						github::ProjectCardContentType::Issue,
					)
					.await?;
			}
		}
		if let Some(matrix_id) = self
			.github_to_matrix
			.get(&local_state.issue_project().unwrap().actor_login)
			.and_then(|matrix_id| matrix::parse_id(matrix_id))
		{
			self.matrix_bot.send_private_message(
				&self.db,
				&matrix_id,
				&ISSUE_REVERT_PROJECT_NOTIFICATION
					.replace("{1}", &issue_html_url),
			)?;
		}
		Ok(())
	}

	fn author_non_special_project_state_confirmed(
		&self,
		local_state: &mut LocalState,
		issue: &github::Issue,
		project: &github::Project,
		project_column: &github::ProjectColumn,
		process_info: &process::ProcessInfo,
		actor: &github::User,
	) -> Result<()> {
		let confirmed_id =
			local_state.issue_project().unwrap().project_column_id;

		let confirmed_matches_last = local_state
			.last_confirmed_issue_project()
			.map(|proj| proj.project_column_id == confirmed_id)
			.unwrap_or(false);

		if !confirmed_matches_last {
			local_state.update_issue_confirm_project_ping(None, &self.db)?;
			local_state.update_last_confirmed_issue_project(
				local_state.issue_project().cloned(),
				&self.db,
			)?;
		}

		if project_column.id != confirmed_id {
			// project has been changed since
			// the confirmation
			local_state.update_issue_confirm_project_ping(
				Some(SystemTime::now()),
				&self.db,
			)?;
			local_state.update_issue_project(
				Some(IssueProject {
					state: IssueProjectState::Unconfirmed,
					actor_login: actor.login.clone(),
					project_column_id: project_column.id,
				}),
				&self.db,
			)?;
			self.matrix_bot.send_to_room(
				&process_info.matrix_room_id,
				&PROJECT_CONFIRMATION
					.replace(
						"{issue_url}",
						issue.html_url.as_ref().context(error::MissingData)?,
					)
					.replace(
						"{project_url}",
						project
							.html_url
							.as_ref()
							.context(error::MissingData)?,
					)
					.replace(
						"{issue_id}",
						&format!("{}", issue.id.context(error::MissingData)?),
					)
					.replace("{column_id}", &format!("{}", project_column.id))
					.replace(
						"{seconds}",
						&format!(
							"{}",
							self.config.project_confirmation_timeout
						),
					),
			)?;
		}
		Ok(())
	}

	pub async fn handle_issue(
		&self,
		projects: &[(Option<github::Project>, process::ProcessInfo)],
		repo: &github::Repository,
		issue: &github::Issue,
	) -> Result<()> {
		let issue_id = issue.id.context(error::MissingData)?;

		let db_key = issue_id.to_le_bytes().to_vec();
		let mut local_state = LocalState::get_or_default(&self.db, db_key)?;

		let author_is_core =
			self.core_devs.iter().any(|u| issue.user.id == u.id);

		if projects.is_empty() {
			// there are no projects matching those listed in Process.toml so do nothing
		} else {
			match self
				.issue_actor_and_project_card(&repo.name, issue.number)
				.await?
			{
				None => {
					log::info!(
                    "Handling issue '{issue_title}' with no project in repo '{repo_name}'",
                    issue_title = issue.title.as_ref().unwrap_or(&"".to_owned()),
                    repo_name = repo.name
                );

					let since = local_state
						.issue_no_project_ping()
						.and_then(|ping| ping.elapsed().ok());
					let special_of_project = projects
						.iter()
						.find(|(_, p)| p.is_special(&issue.user.login))
						.and_then(|(p, pi)| p.as_ref().map(|p| (p, pi)));

					if projects.len() == 1 && special_of_project.is_some() {
						// repo contains only one project and the author is special
						// so we can attach it with high confidence
						self.author_special_attach_only_project(
							&mut local_state,
							issue,
							special_of_project.expect("checked above"),
						)
						.await?;
					} else if author_is_core
						|| projects
							.iter()
							.find(|(_, p)| p.is_special(&issue.user.login))
							.is_some()
					{
						// author is a core developer or special of at least one
						// project in the repo
						self.issue_author_core_no_project(
							&mut local_state,
							issue,
							since,
						)
						.await?;
					} else {
						// author is neither core developer nor special
						self.issue_author_unknown_no_project(
							&mut local_state,
							issue,
							since,
						)
						.await?;
					}
				}
				Some((actor, card)) => {
					let project: github::Project =
						self.github_bot.project(&card).await?;
					let project_column: github::ProjectColumn =
						self.github_bot.project_column(&card).await?;

					log::info!(
                        "Handling issue '{issue_title}' in project '{project_name}' in repo '{repo_name}'",
                        issue_title = issue.title.as_ref().unwrap_or(&"".to_owned()),
                        project_name = project.name,
                        repo_name = repo.name
                );

					if let Some(process_info) = projects
						.iter()
						.find(|(p, _)| {
							p.as_ref()
								.map_or(false, |p| &project.name == &p.name)
						})
						.map(|(_, p)| p)
					{
						if !process_info.is_special(&actor.login) {
							// TODO check if confirmation has confirmed/denied.
							// requires parsing messages in project room

							match local_state.issue_project().map(|p| p.state) {
								None => self
									.author_non_special_project_state_none(
										&mut local_state,
										issue,
										&project,
										&project_column,
										&process_info,
										&actor,
									)?,
								Some(IssueProjectState::Unconfirmed) => self
									.author_non_special_project_state_unconfirmed(
										&mut local_state,
										issue,
										&project,
										&project_column,
										&process_info,
										&actor,
									)
									.await?,
								Some(IssueProjectState::Denied) => self
									.author_non_special_project_state_denied(
										&mut local_state,
										issue,
										&project,
										&project_column,
										&process_info,
										&actor,
									)
									.await?,
								Some(IssueProjectState::Confirmed) => self
									.author_non_special_project_state_confirmed(
										&mut local_state,
										issue,
										&project,
										&project_column,
										&process_info,
										&actor,
									)?,
							};
						} else {
							// actor is special so allow any change
						}
					} else {
						// no key in in Process.toml matches the project name
						// TODO notification here
					}
				}
			}
		}
		Ok(())
	}
}
