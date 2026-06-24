-- Seed initial/default data for a fresh Plex PostgreSQL database.

-- accounts
INSERT INTO plex.accounts (id, name, created_at, updated_at, default_audio_language, default_subtitle_language, auto_select_subtitle, auto_select_audio)
VALUES (1, 'Administrator', 1289520473, 1782210228, '', '', 1, 1);

-- activities
INSERT INTO plex.activities (id, parent_id, type, title, subtitle, scheduled_at, started_at, finished_at, cancelled)
VALUES (1, NULL, 'provider.subscriptions.process', 'Processing subscriptions', '', NULL, 1782210228, 1782210238, 0);
INSERT INTO plex.activities (id, parent_id, type, title, subtitle, scheduled_at, started_at, finished_at, cancelled)
VALUES (2, NULL, 'provider.subscriptions.process', 'Processing subscriptions', '', NULL, 1782210238, 1782210238, 0);

-- devices
INSERT INTO plex.devices (id, identifier, name, created_at, updated_at, platform)
VALUES (1, 'd370b991-6002-4b8b-a4e2-5ec054c4dc4b', '', 1782210228, 1782210228, '');

-- metadata_agent_providers
INSERT INTO plex.metadata_agent_providers (id, identifier, title, uri, agent_type, metadata_types, online, created_at, updated_at, extra_data)
VALUES (1, 'tv.plex.agents.movie', 'Plex Movie', 'provider://tv.plex.provider.metadata', 0, '1', 1, 1782210228, 1782210231, NULL);
INSERT INTO plex.metadata_agent_providers (id, identifier, title, uri, agent_type, metadata_types, online, created_at, updated_at, extra_data)
VALUES (2, 'tv.plex.agents.series', 'Plex Series', 'provider://tv.plex.provider.metadata', 0, '2,3,4', 1, 1782210228, 1782210231, NULL);
INSERT INTO plex.metadata_agent_providers (id, identifier, title, uri, agent_type, metadata_types, online, created_at, updated_at, extra_data)
VALUES (3, 'tv.plex.agents.music', 'Plex Music', 'provider://tv.plex.provider.metadata', 0, '8,9,10', 1, 1782210228, 1782210231, NULL);
INSERT INTO plex.metadata_agent_providers (id, identifier, title, uri, agent_type, metadata_types, online, created_at, updated_at, extra_data)
VALUES (4, 'org.musicbrainz.agents.music', 'MusicBrainz', 'provider://tv.plex.provider.metadata', 2, '8,9,10', 1, 1782210228, 1782210231, NULL);
INSERT INTO plex.metadata_agent_providers (id, identifier, title, uri, agent_type, metadata_types, online, created_at, updated_at, extra_data)
VALUES (5, 'tv.plex.agents.none', 'Plex Personal Media', '', 0, '1,2,3,4,8,9,10,13,14,20,21,22', 1, 1782210228, 1782210228, NULL);
INSERT INTO plex.metadata_agent_providers (id, identifier, title, uri, agent_type, metadata_types, online, created_at, updated_at, extra_data)
VALUES (6, 'tv.plex.agents.localmedia', 'Plex Local Media', '', 1, '1,2,3,4,8,9,10,13,14,20,21,22', 0, 1782210228, 1782210228, NULL);

-- metadata_agent_provider_groups
INSERT INTO plex.metadata_agent_provider_groups (id, title, primary_identifier, created_at, updated_at, extra_data)
VALUES (1, 'Plex Movie', 'tv.plex.agents.movie', 1782210228, 1782210228, '{"at:builtIn":"1","url":"at%3AbuiltIn=1"}');
INSERT INTO plex.metadata_agent_provider_groups (id, title, primary_identifier, created_at, updated_at, extra_data)
VALUES (2, 'Plex Series', 'tv.plex.agents.series', 1782210228, 1782210228, '{"at:builtIn":"1","url":"at%3AbuiltIn=1"}');
INSERT INTO plex.metadata_agent_provider_groups (id, title, primary_identifier, created_at, updated_at, extra_data)
VALUES (3, 'Plex Music', 'tv.plex.agents.music', 1782210228, 1782210228, '{"at:builtIn":"1","url":"at%3AbuiltIn=1"}');
INSERT INTO plex.metadata_agent_provider_groups (id, title, primary_identifier, created_at, updated_at, extra_data)
VALUES (4, 'Plex Personal Media', 'tv.plex.agents.none', 1782210228, 1782210228, '{"at:builtIn":"1","url":"at%3AbuiltIn=1"}');

-- metadata_agent_provider_group_items
INSERT INTO plex.metadata_agent_provider_group_items (id, metadata_agent_provider_group_id, metadata_agent_provider_id, "order")
VALUES (1, 1, 1, 1000.0);
INSERT INTO plex.metadata_agent_provider_group_items (id, metadata_agent_provider_group_id, metadata_agent_provider_id, "order")
VALUES (2, 2, 2, 1000.0);
INSERT INTO plex.metadata_agent_provider_group_items (id, metadata_agent_provider_group_id, metadata_agent_provider_id, "order")
VALUES (3, 3, 3, 1000.0);
INSERT INTO plex.metadata_agent_provider_group_items (id, metadata_agent_provider_group_id, metadata_agent_provider_id, "order")
VALUES (4, 4, 5, 1000.0);

-- plugin_prefixes
INSERT INTO plex.plugin_prefixes (id, plugin_id, name, prefix, art_url, thumb_url, titlebar_url, share, has_store_services, prefs)
VALUES (3, 1, 'Player', '/player', '', '/:/plugins/com.plexapp.system/resources/icon-default.png?t=1777730092', '', 0, 0, 1);
INSERT INTO plex.plugin_prefixes (id, plugin_id, name, prefix, art_url, thumb_url, titlebar_url, share, has_store_services, prefs)
VALUES (4, 1, 'System', '/system', '', '/:/plugins/com.plexapp.system/resources/icon-default.png?t=1777730092', '', 0, 0, 1);

-- plugins
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (1, 'com.plexapp.system', 2, NULL, 1782210228, 1782210228, 1777730092);
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (2, 'com.plexapp.agents.lyricfind', 2, NULL, 1782210229, 1782210228, 1777730092);
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (3, 'com.plexapp.agents.none', 2, NULL, 1782210229, 1782210228, 1777730092);
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (4, 'com.plexapp.agents.plexthememusic', 2, NULL, 1782210229, 1782210228, 1777730092);
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (5, 'org.musicbrainz.agents.music', 2, NULL, 1782210230, 1782210229, 1777730092);
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (6, 'com.plexapp.agents.lastfm', 2, NULL, 1782210250, 1782210229, 1777730092);
INSERT INTO plex.plugins (id, identifier, framework_version, access_count, installed_at, accessed_at, modified_at)
VALUES (7, 'com.plexapp.agents.themoviedb', 2, NULL, 1782210250, 1782210229, 1777730092);

-- preferences
INSERT INTO plex.preferences (id, name, value)
VALUES (2, 'SyncedNeedsChangedAtUpdate', '0');

-- tags
INSERT INTO plex.tags (id, metadata_item_id, tag, tag_type, user_thumb_url, user_art_url, user_music_url, created_at, updated_at, tag_value, extra_data, key, parent_id)
VALUES (1, NULL, 'Optimized for Mobile', 42, '', '', '', 1782210228, 1782210228, NULL, '{"sr:deviceProfile":"Universal Mobile","sr:mediaSettings":"advancedSubtitles=burn&autoAdjustQuality=0&autoAdjustSubtitle=0&boostDialog=0&directPlay=1&directStream=1&directStreamAudio=1&normalizeLoudness=0&subtitles=auto&videoBitrate=4000&videoQuality=74&videoResolution=1280x720","url":"sr%3AdeviceProfile=Universal%20Mobile&sr%3AmediaSettings=advancedSubtitles%3Dburn%26autoAdjustQuality%3D0%26autoAdjustSubtitle%3D0%26boostDialog%3D0%26directPlay%3D1%26directStream%3D1%26directStreamAudio%3D1%26normalizeLoudness%3D0%26subtitles%3Dauto%26videoBitrate%3D4000%26videoQuality%3D74%26videoResolution%3D1280x720"}', '', NULL);
INSERT INTO plex.tags (id, metadata_item_id, tag, tag_type, user_thumb_url, user_art_url, user_music_url, created_at, updated_at, tag_value, extra_data, key, parent_id)
VALUES (2, NULL, 'Optimized for TV', 42, '', '', '', 1782210228, 1782210228, NULL, '{"sr:deviceProfile":"Universal TV","sr:mediaSettings":"advancedSubtitles=burn&autoAdjustQuality=0&autoAdjustSubtitle=0&boostDialog=0&directPlay=1&directStream=1&directStreamAudio=1&normalizeLoudness=0&subtitles=auto&videoBitrate=8000&videoQuality=99&videoResolution=1920x1080","url":"sr%3AdeviceProfile=Universal%20TV&sr%3AmediaSettings=advancedSubtitles%3Dburn%26autoAdjustQuality%3D0%26autoAdjustSubtitle%3D0%26boostDialog%3D0%26directPlay%3D1%26directStream%3D1%26directStreamAudio%3D1%26normalizeLoudness%3D0%26subtitles%3Dauto%26videoBitrate%3D8000%26videoQuality%3D99%26videoResolution%3D1920x1080"}', '', NULL);
INSERT INTO plex.tags (id, metadata_item_id, tag, tag_type, user_thumb_url, user_art_url, user_music_url, created_at, updated_at, tag_value, extra_data, key, parent_id)
VALUES (3, NULL, 'Original Quality', 42, '', '', '', 1782210228, 1782210228, NULL, '{"sr:deviceProfile":"Universal TV","sr:mediaSettings":"advancedSubtitles=burn&autoAdjustQuality=0&autoAdjustSubtitle=0&boostDialog=0&directPlay=1&directStream=1&directStreamAudio=1&normalizeLoudness=0&subtitles=auto","url":"sr%3AdeviceProfile=Universal%20TV&sr%3AmediaSettings=advancedSubtitles%3Dburn%26autoAdjustQuality%3D0%26autoAdjustSubtitle%3D0%26boostDialog%3D0%26directPlay%3D1%26directStream%3D1%26directStreamAudio%3D1%26normalizeLoudness%3D0%26subtitles%3Dauto"}', '', NULL);

-- Reset serial sequences so future inserts don't collide.
SELECT setval('plex.accounts_id_seq', COALESCE((SELECT max(id) FROM plex.accounts), 1));
SELECT setval('plex.activities_id_seq', COALESCE((SELECT max(id) FROM plex.activities), 1));
SELECT setval('plex.devices_id_seq', COALESCE((SELECT max(id) FROM plex.devices), 1));
SELECT setval('plex.metadata_agent_providers_id_seq', COALESCE((SELECT max(id) FROM plex.metadata_agent_providers), 1));
SELECT setval('plex.metadata_agent_provider_groups_id_seq', COALESCE((SELECT max(id) FROM plex.metadata_agent_provider_groups), 1));
SELECT setval('plex.metadata_agent_provider_group_items_id_seq', COALESCE((SELECT max(id) FROM plex.metadata_agent_provider_group_items), 1));
SELECT setval('plex.plugin_prefixes_id_seq', COALESCE((SELECT max(id) FROM plex.plugin_prefixes), 1));
SELECT setval('plex.plugins_id_seq', COALESCE((SELECT max(id) FROM plex.plugins), 1));
SELECT setval('plex.preferences_id_seq', COALESCE((SELECT max(id) FROM plex.preferences), 1));
SELECT setval('plex.tags_id_seq', COALESCE((SELECT max(id) FROM plex.tags), 1));
